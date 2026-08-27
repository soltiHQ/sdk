---
title: Observe an agent
description: Connect application logging, SDK metrics, and a supervised scrape endpoint without confusing telemetry with task outcomes.
---

# Observe an agent

`solti-observe` configures process logging. `solti-prometheus` connects metrics from other crates to an application-owned registry.
Neither replaces task state, direct task outcomes, or an output store.

## Separate the data paths

| Question | Path |
|----------|------|
| What did application or SDK code report? | `tracing` records through the logger installed by `solti-observe`. |
| How many operations, failures, or observed lifecycle events occurred? | Source-crate callbacks or Taskvisor events through Prometheus adapters. |
| Which logical phases are retained now? | `PrometheusCoreStateCollector` reads `solti-core::TaskState` on collection. |
| What did a workload write to stdout or stderr? | SDK [task output and history](output-and-history.md), not the logger configuration. |
| Did shutdown and cleanup finish? | The owned [shutdown operation](cancellation-and-shutdown.md), not a log message or metric. |

```text
application + SDK tracing ──► solti-observe ──► text / JSON / journald

runner / API / discovery callbacks ──┐
Taskvisor best-effort events ────────┼──► solti-prometheus ──► shared Registry
core phase snapshot + build/process ┘                               │
                                                                    ▼
                                                     supervised GET /metrics
```

The binary owns logger initialization, registry lifetime, adapter injection, endpoint reachability, and shutdown.
Each source crate owns the behavior being measured.

## Install one process logger

No optional feature is needed for text or JSON output:

```rust
use solti_observe::{LoggerConfig, LoggerFormat, LoggerLevel, init_logger};

fn initialize_logging() -> Result<(), solti_observe::LoggerError> {
    init_logger(&LoggerConfig {
        format: LoggerFormat::Json,
        level: LoggerLevel::new("solti_core=debug,info")?,
        with_targets: true,
        use_color: false,
        ..Default::default()
    })?;

    tracing::info!(event = "service.logger_ready", "logger installed");
    Ok(())
}
```

Call this once near process startup. Creating `LoggerConfig` alone installs nothing.
`init_logger` installs a process-global `tracing` subscriber; a second installation returns an error.
It is not a logger handle owned by `SupervisorApi` and is not replaced by applying a Task.
The snippet also needs the application's `tracing` dependency.

| Setting | Default | Meaning |
|---------|---------|---------|
| `format` | Text | Text or JSON; journald is optional. |
| `level` | `info` | A validated `tracing_subscriber::EnvFilter` expression. |
| `timezone` | UTC | UTC or cached local offset for text and JSON timestamps. |
| `with_targets` | `true` | Include event targets in text and JSON. |
| `use_color` | `true` | ANSI only for text when stdout is an interactive terminal. |

Serde fills missing fields from these defaults and rejects unknown fields.
JSON never uses ANSI colors. Text and JSON timestamps are RFC 3339.
Journald owns its native record format; timezone, target, and color fields do not configure that layer.

| Optional integration | `solti-observe` feature | `solti` facade feature |
|----------------------|------------------------|------------------------|
| Native journal output on Linux | `journald` | `observe-journald` |
| Forward `log` records into `tracing` | `log-compat` | `observe-log-compat` |
| Supervised local-offset refresh | `timezone-sync` | `observe-timezone-sync` |

The direct `full` feature enables all three; none is enabled by default.
Journald initialization is unavailable on non-Linux targets.
Installing a logger does not itself subscribe to Taskvisor's lifecycle event bus.
Use a Taskvisor subscriber when that event stream is needed; logger records and subscriber events are separate inputs.

## Refresh local timestamps explicitly

For local text or JSON timestamps, `init_logger` detects the current UTC offset before subscriber installation.
Detection failure returns `LocalOffsetUnavailable`; it does not silently select UTC.
The offset then stays cached unless refreshed.

With `timezone-sync`, the application can place refresh under core supervision:

```rust
use solti_core::SupervisorApi;

async fn add_timezone_refresh(supervisor: &SupervisorApi) -> Result<(), solti_core::CoreError> {
    let (manifest, task_ref) = solti_observe::timezone_sync();
    supervisor.create_embedded_task(manifest, task_ref).await?;
    Ok(())
}
```

The task uses name and slot `solti-observe-timezone-sync`, Embedded execution, and `Replace` admission.
Each successful attempt updates the shared offset cache. The next attempt follows one hour after success.
Failed detection is retryable with equal-jitter backoff from 5 seconds to 5 minutes, factor 2.
Each attempt has a 60-second timeout.
This is a separate maintenance resource, not an automatic effect of `init_logger`.
UTC output does not need local-offset refresh.

## Connect metrics producers

No `solti-prometheus` integration is enabled by default.
`Registry`, `Error`, and `register_build_info` are always available.

| Producer | Adapter and injection point | Direct feature | `solti` facade feature |
|----------|-----------------------------|----------------|------------------------|
| Runner setup and cleanup errors | `PrometheusRunnerMetrics` → `BuildContext::with_metrics` → router | `runner` | `prometheus-runner` |
| API request lifecycle | `PrometheusApiMetrics` → `HttpApi::with_metrics` or `GrpcApi::with_metrics` | `api` | `prometheus-api` |
| Discovery requests and holds | `PrometheusDiscoverMetrics` → `DiscoverConfigBuilder::with_metrics` | `discover` | `prometheus-discover` |
| Taskvisor lifecycle events | `PrometheusTaskvisorSubscriber` → core builder `with_subscribers` | `taskvisor` | `prometheus-taskvisor` |
| Controller events | Additional metrics on the same Taskvisor subscriber | `taskvisor-controller` | `prometheus-taskvisor-controller` |
| Current stored task phases | Register `PrometheusCoreStateCollector::new(supervisor.state())` | `state` | `prometheus-state` |
| Current Linux process | `register_process_collector(&registry)` | `process` | `prometheus-process` |
| HTTP exposition | `server` or `server_with_config`, submitted as Embedded work | `server` | `prometheus-server` |

The direct `full` feature enables all integrations.
The facade `prometheus` feature is narrower: base registry, runner metrics, and Taskvisor/controller metrics.
Use explicit facade features or `prometheus-full` for the other adapters.
Enabling an adapter feature does not attach an instance to a producer or start a server.

## Keep one registry

Create the registry at the binary's composition root and register each metric group once:

```rust
use std::sync::Arc;
use solti_prometheus::{Registry, register_build_info};

fn metrics_registry() -> Result<Arc<Registry>, solti_prometheus::Error> {
    let registry = Arc::new(Registry::new());
    register_build_info(
        &registry,
        &[("version", env!("CARGO_PKG_VERSION"))],
    )?;
    Ok(registry)
}
```

Adapter constructors register their collectors immediately.
A descriptor conflict rejects the complete adapter group without leaving a partially registered group.
Registering the same group twice returns `AlreadyReg`.
The state collector is the exception: construct it, then explicitly call `registry.register`.

For runner callbacks, install the metrics handle in the build context before using it for routing:

```rust
use std::sync::Arc;
use solti_prometheus::{PrometheusRunnerMetrics, Registry};
use solti_runner::{BuildContext, MetricsHandle, RunnerRouter};

fn measured_router(registry: &Registry) -> Result<RunnerRouter, solti_prometheus::Error> {
    let metrics: MetricsHandle = Arc::new(PrometheusRunnerMetrics::new(registry)?);
    let context = BuildContext::default().with_metrics(metrics);
    Ok(RunnerRouter::new().with_context(context))
}
```

Register the application's runners on this router and retain their lifecycle handles.
Then install the Taskvisor subscriber before core starts, and register the state collector against that running core:

```rust
use std::sync::Arc;
use solti_core::SupervisorApi;
use solti_prometheus::{PrometheusCoreStateCollector, PrometheusTaskvisorSubscriber, Registry};
use solti_runner::RunnerRouter;
use taskvisor::Subscribe;

async fn start_observed_core(
    router: RunnerRouter,
    registry: &Registry,
) -> Result<SupervisorApi, Box<dyn std::error::Error>> {
    let subscriber: Arc<dyn Subscribe> =
        Arc::new(PrometheusTaskvisorSubscriber::new(registry)?);
    let supervisor = SupervisorApi::builder(router)
        .with_subscribers(vec![subscriber])
        .start()
        .await?;

    registry.register(Box::new(PrometheusCoreStateCollector::new(
        supervisor.state(),
    )?))?;
    Ok(supervisor)
}
```

These snippets require the corresponding direct `runner`, `taskvisor`, and `state` Prometheus features.
The complete [operations example](../crates/solti/examples/operations_prometheus.rs) connects them to real subprocess work and owns shutdown.

API and discovery adapters use the same registry but separate injection points:

```text
PrometheusApiMetrics::new(&registry)
    └── Arc ──► HttpApi::with_metrics / GrpcApi::with_metrics

PrometheusDiscoverMetrics::new(&registry)
    └── Arc ──► DiscoverConfigBuilder::with_metrics
```

## Interpret each metric at its source

- `solti_runner_errors_total` counts reported setup and cleanup errors, not every task failure. Custom runner and error strings become label values; the application controls their cardinality.
- API metrics describe transport requests. Paths supplied by the SDK use HTTP route templates or full gRPC method names, not individual task names.
- API streams remain in flight until completion, failure, or drop. An early drop still decrements in-flight state, but does not record a completed request or duration.
- Discovery duration covers the transport attempt, not startup jitter or a preceding retry hold. Failure labels come from `DiscoverFailReason`, not remote diagnostic text.
- Taskvisor counters and histograms describe delivered best-effort events. They are not a durable audit log or direct final-outcome channel.
- `solti_core_tasks_by_phase` reads current retained SDK state at collection time, including retained terminal tasks and internal Embedded tasks. It is not a physical-process count.
- `register_process_collector` registers current-process metrics on Linux and is a no-op elsewhere.

The Taskvisor subscriber correlates delivered attempts by Taskvisor ID and attempt number.
An overflow attributed to it or the shared `subscriber_listener`, or a conflicting active attempt identity, permanently invalidates its in-flight tracking.
It clears tracking and exports `solti_taskvisor_attempts_in_flight=NaN`; later counters and histograms still update.
An overflow for another subscriber does not invalidate this subscriber.
The default subscriber queue capacity is 2048 and can be changed through `with_queue_capacity`.

Runner build contexts, API builders, and discovery configuration install sticky panic containment around configured metrics callbacks.
After the first observed backend panic, later calls through that boundary drop updates without invoking the backend again.
Calls already running concurrently can still finish or panic.
This does not serialize callbacks, protect direct application calls to the backend, or isolate `panic = "abort"`.
Metrics backends remain responsible for non-panicking behavior.

## Serve the registry

The server factory constructs Embedded work. Binding starts only when Taskvisor runs its attempt:

```rust
use std::sync::Arc;
use solti_core::SupervisorApi;
use solti_prometheus::{MetricsServerConfig, Registry, server_with_config};

async fn add_metrics_endpoint(
    supervisor: &SupervisorApi,
    registry: Arc<Registry>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (manifest, task_ref) = server_with_config(
        registry,
        "127.0.0.1:9090",
        "agent-metrics-v1",
        MetricsServerConfig::default(),
    )?;
    supervisor.create_embedded_task(manifest, task_ref).await?;
    Ok(())
}
```

This needs `solti-prometheus/server` in addition to the chosen collectors.
The example revision identifies the application's registry intent.
The factory also includes the listen address and effective scrape settings in the Embedded revision.
Use a new caller revision when replacing captured registry intent that those values do not identify.

The task uses name and slot `solti-metrics-server`, `Replace` admission, and an always-restart policy.
Bind and serve failures are retryable with equal-jitter backoff from 1 to 30 seconds, factor 2.
Cancellation requests graceful HTTP shutdown.
The application still owns joining core shutdown and any runner-specific cleanup.

The endpoint serves only `GET /metrics`. `HEAD /metrics` returns `405` without gathering.
It is plaintext and unauthenticated; Task API access-control hooks do not apply to it.
The application owns a controlled bind address or an appropriate external network/authenticated-TLS boundary.

## Bound scrapes, not arbitrary collector code

| `MetricsServerConfig` setting | Default | Accepted range |
|------------------------------|---------|----------------|
| Physical scrape ownerships | 2 | 1–16 |
| Encoded response bytes | 4 MiB | 1 byte–64 MiB |
| Gather and encoding response deadline | 10 seconds | 1 ms–60 seconds |

Use `try_with_max_concurrent_scrapes`, `try_with_max_response_bytes`, and `try_with_scrape_timeout` for checked overrides.
These values are endpoint limits, not a recommendation for every workload.

| Outcome | HTTP response |
|---------|---------------|
| Complete exposition | `200` |
| Every ownership slot is occupied | `503` with `Retry-After: 1` |
| Gather/encoding deadline elapsed | `504` |
| Oversized response, encoding failure, or unwinding collector panic | `500` |

Gathering and encoding run on Tokio's blocking pool.
The response-byte limit rejects an oversized exposition in full; it never sends a truncated document.
It does not bound allocations inside a collector, the gathered metric graph, or temporary encoder values.
Only register trusted collectors with bounded work and allocation behavior.

A response timeout or disconnected client cannot interrupt synchronous collector code.
The physical job keeps its slot until it returns.
A successful job transfers the slot to the encoded response bytes; it is released only after their final owner drops, including transport-held clones.
Slow clients therefore remain charged for retained response bodies.
An HTTP `504` is not proof that collector work ended.

The endpoint reports scrape outcomes through structured tracing rather than registering recursive self-metrics in the registry.
Unwinding collector panics can become `500`; an aborting panic cannot be isolated.
The detailed [server contract](../crates/solti-prometheus/src/server.rs) also describes containment for tracing and hostile panic payloads.

Source: [logger installation](../crates/solti-observe/src/logger/log.rs), [timezone refresh](../crates/solti-observe/src/logger/tasks/timezone_sync.rs), [runner build context](../crates/solti-runner/src/context.rs), [metrics features](../crates/solti-prometheus/Cargo.toml), [Taskvisor subscriber](../crates/solti-prometheus/src/subscriber.rs), [state collector](../crates/solti-prometheus/src/state.rs), [API metrics contract](../crates/solti-api/src/metrics.rs), and [scrape server](../crates/solti-prometheus/src/server.rs).

## Run the examples

Small examples isolate each setup boundary:

```sh
cargo run -p solti-observe --example text_logging
cargo run -p solti-observe --example json_logging
cargo run -p solti-observe --example timezone_sync --features timezone-sync
cargo run -p solti-prometheus --example shared_registry
cargo run -p solti-prometheus --example adapter_metrics --features api,discover,runner
cargo run -p solti-prometheus --example metrics_server --features server
```

Text and JSON run in separate processes because only one global logger installation can succeed.
The [timezone example](../crates/solti-observe/examples/timezone_sync.rs) and [metrics server example](../crates/solti-prometheus/examples/metrics_server.rs) stop at task construction; they do not start a supervisor or a listener.

Full compositions attach the integrations to core and routed work:

```sh
cargo run -p solti --example operations_observe \
  --features core,exec-subprocess,observe-timezone-sync
cargo run -p solti --example operations_prometheus \
  --features core,exec-subprocess,prometheus,prometheus-server,prometheus-state
```

The [logging composition](../crates/solti/examples/operations_observe.rs) uses local timestamps, a timezone maintenance task, and one subprocess.
The [metrics composition](../crates/solti/examples/operations_prometheus.rs) records actual runner and Taskvisor activity, serves `/metrics`, and stays active until Ctrl-C.
`SOLTI_METRICS_ADDR` overrides its default `127.0.0.1:9090` address.
See the [example catalog](example-catalog.md) for their wider requirements and [production boundaries](production-boundaries.md) for the limits of observed state.
