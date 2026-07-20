# solti-observe

> Structured logging and timezone support for Solti agents.

`solti-observe` is the logging crate for the Solti SDK.
It installs a `tracing` subscriber and keeps logger config in small value types.

Use it when you build an agent binary and want one clear place for logs and local timestamps.

## The logging setup you stop repeating

Most agents need the same boot code:

```rust,ignore
// choose text/json/journald
// choose level filter
// decide UTC or local timestamps
// install tracing once
```

With `solti-observe`, this becomes a `LoggerConfig` plus one call:

```rust,no_run
use solti_observe::{LoggerConfig, LoggerLevel, init_logger};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = LoggerConfig {
        level: LoggerLevel::new("solti_core=debug,info")?,
        ..Default::default()
    };

    init_logger(&config)?;
    tracing::info!("agent ready");
    Ok(())
}
```

`init_logger` installs the global tracing subscriber. It can succeed only once in a process.

## Quick Start

### Text Logs

Text logs are the default. They are best for local development and service logs read by people:

```rust,no_run
use solti_observe::{LoggerConfig, init_logger};

fn main() -> Result<(), solti_observe::LoggerError> {
    init_logger(&LoggerConfig::default())?;
    tracing::info!("ready");
    Ok(())
}
```

### JSON Logs

JSON logs are useful for Loki, ELK, or another log pipeline:

```rust,no_run
use solti_observe::{LoggerConfig, LoggerFormat, init_logger};

fn main() -> Result<(), solti_observe::LoggerError> {
    let config = LoggerConfig {
        format: LoggerFormat::Json,
        with_targets: true,
        ..Default::default()
    };

    init_logger(&config)?;
    Ok(())
}
```

### Config From JSON

`LoggerConfig` supports serde. Missing fields use defaults:

```rust
use solti_observe::{LoggerConfig, LoggerFormat};

let config: LoggerConfig = serde_json::from_str(r#"{
    "format": "json",
    "level": "taskvisor=debug,info",
    "with_targets": true
}"#).unwrap();

assert_eq!(config.format, LoggerFormat::Json);
assert_eq!(config.level.as_str(), "taskvisor=debug,info");
```

### Local Timestamps

If you want local timestamps, call `init_local_offset` before Tokio starts worker threads:

```rust,no_run
use solti_observe::{
    LoggerConfig, LoggerTimeZone, init_local_offset, init_logger,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_local_offset();

    tokio::runtime::Runtime::new()?.block_on(async {
        let config = LoggerConfig {
            tz: LoggerTimeZone::Local,
            ..Default::default()
        };

        init_logger(&config)?;
        tracing::info!("local timestamps are ready");
        Ok(())
    })
}
```

UTC is the default and always works.

## What Ships

| Item                | Feature         | Use it for                                 |
|---------------------|-----------------|--------------------------------------------|
| `LoggerConfig`      | always          | Logger settings with serde defaults        |
| `LoggerFormat`      | always          | `text`, `json`, or `journald`              |
| `LoggerLevel`       | always          | Validated `EnvFilter` strings              |
| `LoggerTimeZone`    | always          | `utc` or `local` timestamps                |
| `init_logger`       | always          | Install the global tracing subscriber      |
| `init_local_offset` | always          | Cache local UTC offset before Tokio starts |
| `timezone_sync`     | `timezone-sync` | Build a supervised offset refresh task     |

## Core Model

```text
LoggerConfig
  |
  v
init_logger()
  |
  |-- text logger
  |-- JSON logger
  |-- journald logger (Linux only)
  |
  v
tracing macros
```

The optional timezone task plugs into the same process:

```text
timezone_sync task -- refresh attempt --> local offset cache
```

## Logger Config

Defaults are conservative:

| Field          | Default  | Meaning                                   |
|----------------|----------|-------------------------------------------|
| `format`       | `Text`   | Human-readable logs                       |
| `level`        | `info`   | Global filter expression                  |
| `tz`           | `Utc`    | UTC timestamps                            |
| `with_targets` | `true`   | Include module targets                    |
| `use_color`    | `true`   | Use colors only when stdout is a terminal |

`LoggerLevel` accepts the same syntax as `tracing_subscriber::EnvFilter`:

```rust
use solti_observe::LoggerLevel;

let level = LoggerLevel::new("solti_exec=trace,taskvisor=debug,info").unwrap();
assert_eq!(level.as_str(), "solti_exec=trace,taskvisor=debug,info");
```

## Formats

| Format     | Output          | Notes                                        |
|------------|-----------------|----------------------------------------------|
| `Text`     | formatted lines | Best for development and normal service logs |
| `Json`     | structured JSON | Best for log collectors                      |
| `Journald` | systemd journal | Linux only                                   |

On non-Linux platforms, parsing or using `journald` returns `LoggerError::JournaldNotSupported`.

## Timezone Sync Task

Enable the `timezone-sync` feature to build a periodic supervised task:

```rust,ignore
use solti_observe::timezone_sync;

let (task, spec) = timezone_sync();
supervisor.submit_with_task(task, &spec).await?;
```

The task uses slot `solti-logger-tz-sync`, `AdmissionPolicy::Replace`, a 1 hour success period, and a defensive 5 second to 5 minute backoff.

In practice, local offset detection often works only before Tokio starts worker threads. So the important call is still `init_local_offset()` during process startup. The sync task is best-effort and useful on platforms where re-detection is allowed later.

## Feature Flags

| Flag            | Default  | Effect                   |
|-----------------|----------|--------------------------|
| `timezone-sync` | off      | Expose `timezone_sync()` |

## Error Model

| Error                  | When it happens                                            |
|------------------------|------------------------------------------------------------|
| `InvalidFormat`        | format is not `text`, `json`, or `journald`                |
| `JournaldNotSupported` | journald was requested outside Linux                       |
| `JournaldInitFailed`   | systemd journal setup failed                               |
| `AlreadyInitialized`   | `init_logger` was called after a subscriber already exists |
| `InvalidTimeZone`      | timezone is not `utc` or `local`                           |
| `InvalidLevel`         | `EnvFilter` could not parse the level string               |

## Notes

- Call `init_logger` once, near process start.
- Call `init_local_offset` before creating a Tokio runtime when `LoggerTimeZone::Local` is used.
- Use `solti-prometheus` for metrics. This crate is for logging and timezone support.
