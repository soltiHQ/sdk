# solti-observe
Observability primitives for the solti task execution system.

Wires [`tracing`](https://docs.rs/tracing) into solti: logger initialization, supervision event logging, and timezone sync.

## Quick start
```rust,no_run
use solti_observe::{LoggerConfig, LoggerLevel, init_local_offset, init_logger};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1) Must run before the tokio runtime spawns threads.
    init_local_offset();

    tokio::runtime::Runtime::new()?.block_on(async {
        // 2) Install the global tracing subscriber.
        let cfg = LoggerConfig {
            level: LoggerLevel::new("info")?,
            ..Default::default()
        };
        init_logger(&cfg)?;

        tracing::info!("ready");
        Ok(())
    })
}
```

## Architecture
```text
  main()
  ├─ init_local_offset()          before tokio runtime (single-threaded)
  └─ tokio::Runtime::new()
      └─ async_main()
          ├─ init_logger(&cfg)    installs global tracing subscriber
          │   ├─ Text  → fmt::Layer (colored, RFC 3339 timestamps)
          │   ├─ Json  → fmt::Layer::json()
          │   └─ Journald → tracing_journald::layer() (Linux only)
          │
          ├─ TracingBridge              (feature: subscriber)
          │   └─ on_event() → trace!/debug!/info!/warn!/error!
          │
          └─ timezone_sync()            (feature: timezone-sync)
              └─ periodic re-detection of local UTC offset
```

## Logger formats

| Format     | Backend                          | Use case                         |
|------------|----------------------------------|----------------------------------|
| `Text`     | `tracing_subscriber::fmt`        | Local development, human reading |
| `Json`     | `tracing_subscriber::fmt::json`  | Log aggregation (ELK, Loki)      |
| `Journald` | `tracing_journald`               | systemd services (Linux only)    |

## Configuration

| Field          | Default | Description                               |
|----------------|---------|-------------------------------------------|
| `format`       | `Text`  | Human-readable colored output             |
| `level`        | `info`  | `EnvFilter` expression                    |
| `tz`           | `Utc`   | Timestamp timezone                        |
| `with_targets` | `true`  | Include module/target names in output     |
| `use_color`    | `true`  | Colored output (auto-disabled if not TTY) |

Supports serde deserialization with missing-field defaults:
```text
{}                                → all defaults
{"level": "debug"}                → debug level, rest defaults
{"format": "json", "tz": "local"} → JSON with local timestamps
```

## Event logging (feature `subscriber`)

The feature re-exports `TracingBridge` from taskvisor.
It forwards every supervision event to tracing under target `taskvisor`, with a stable `event` label and structured fields (`seq`, `id`, `task`, `attempt`, `reason`, `delay_ms`, `timeout_ms`, `duration_ms`, `exit_code`, `backoff_source`).
Failures arrive as ERROR, timeouts and permanent retry give-ups as WARN, milestones as INFO, chatty events as DEBUG.
See the [taskvisor docs](https://docs.rs/taskvisor) for the full level mapping.

## Local timezone support
```text
  Problem:  UtcOffset::current_local_offset() reads /etc/localtime
            which is unsafe in multi-threaded processes (tokio)

  Solution: init_local_offset()    call in main() before Runtime::new() -
                                   this is where the offset is actually captured
            timezone_sync()        periodic re-detection (best effort, see below)

  Fallback: if init_local_offset() is not called, the first timestamp runs a
            one-shot detection; under a running runtime it fails → UTC + stderr warning
```

## Timezone sync task (feature `timezone-sync`)

`timezone_sync()` returns a `(TaskRef, TaskSpec)` pair: a periodic task
(1 hour period via `RestartPolicy::periodic`, `AdmissionPolicy::Replace` in the
`solti-logger-tz-sync` slot) that re-runs local offset detection.

```text
  Attempt outcome     What happens
  ───────────────     ────────────
  Detection succeeds  cache updated, offset change logged at DEBUG
  Detection fails     skipped with a DEBUG log - the task still returns Ok
  Duplicate submit    running instance is replaced (AdmissionPolicy::Replace)
```

In practice detection succeeds only while the process is single-threaded
(a `time` 0.3 restriction), so under a multi-threaded tokio runtime nearly
every attempt is a skip and the effective offset stays the one captured by
`init_local_offset()` at startup. Because the task body never returns an
error, the configured backoff (5 s → 5 min exponential, equal jitter) is
defensive only - no current failure path exercises it.

## Feature flags

| Flag            | Default | Dependencies                             | Effect                          |
|-----------------|---------|------------------------------------------|---------------------------------|
| `subscriber`    | off     | `taskvisor` (+ its `tracing` feature)    | `TracingBridge` re-export       |
| `timezone-sync` | off     | `taskvisor`, `solti-model`               | timezone_sync() periodic task   |

## Key types

| Type                     | Purpose                                                   |
|--------------------------|-----------------------------------------------------------|
| `LoggerConfig`           | Logger configuration (serde-deserializable)               |
| `LoggerFormat`           | Output format: `Text`, `Json`, `Journald`                 |
| `LoggerLevel`            | Validated `EnvFilter` expression wrapper                  |
| `LoggerTimeZone`         | Timestamp timezone: `Utc`, `Local`                        |
| `LoggerError`            | Error type for initialization and parsing                 |
| `TracingBridge`          | Logs taskvisor events via tracing (feature: `subscriber`) |

## Error model
```text
  Variant                When
  ───────                ────
  InvalidFormat          unknown format string (not text/json/journald)
  JournaldNotSupported   journald on non-Linux platform
  JournaldInitFailed     journald layer init error (Linux)
  AlreadyInitialized     init_logger() called twice
  InvalidTimeZone        unknown timezone string (not utc/local)
  InvalidLevel           invalid EnvFilter expression
```

## Notes
- `init_logger` can only be called **once** per process - subsequent calls return `AlreadyInitialized`.
- `init_local_offset` is safe to call multiple times: each call re-runs detection and overwrites the cached offset.
- `TracingBridge` lives in taskvisor; this crate only re-exports it. Level policy and fields evolve upstream.
