//! # JSON logging
//!
//! A serialized configuration selects structured JSON output.
//! Every accepted tracing event becomes one JSON object.
//!
//! This example shows:
//!
//! - Serde input with defaults for omitted fields;
//! - JSON logger installation;
//! - info-level filtering;
//! - structured event fields;
//! - JSON output without ANSI escape sequences.
//!
//! Run with `cargo run -p solti-observe --example json_logging`.

use solti_observe::{LoggerConfig, LoggerFormat, LoggerTimeZone, init_logger};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-observe: JSON logging

  serialized settings ──► LoggerConfig ──► init_logger()
                                                ├──► validate EnvFilter
                                                ├──► build JSON layer
                                                └──► install global subscriber
  structured tracing event ──► info filter ────────────────────┤
                                                               ▼
                                                      one JSON object
                                                          ├── timestamp
                                                          ├── level
                                                          └── fields

  JSON disables ANSI colors.
  The final output line is the structured event produced by the logger.
"#;

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Load logger settings from serialized input and emit machine-readable events."
    );

    let source = r#"{
        "format": "json",
        "level": "info",
        "with_targets": false
    }"#;
    let config: LoggerConfig = serde_json::from_str(source)?;
    assert_eq!(config.format, LoggerFormat::Json);
    assert_eq!(config.timezone, LoggerTimeZone::Utc);
    assert!(config.use_color);
    println!("[config] Parsed format=json, level=info, targets=false.");
    println!("[defaults] Omitted timezone becomes UTC; omitted use_color becomes true.");
    println!("[format] JSON ignores use_color and emits no ANSI escape sequences.");

    init_logger(&config)?;
    println!("[installation] The process-global JSON subscriber is active.");
    println!("[events] A debug event is filtered; the following info event becomes JSON:");

    tracing::debug!(target: "example::api", request_id = 41_u64, "not exported");
    tracing::info!(
        target: "example::api",
        request_id = 42_u64,
        method = "GET",
        path = "/health",
        status = 200_u16,
        "request completed"
    );

    Ok(())
}
