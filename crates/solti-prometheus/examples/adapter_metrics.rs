//! # Metrics adapters
//!
//! Solti crates own their metrics traits.
//! `solti-prometheus` implements those traits against one shared registry.
//!
//! This example shows:
//!
//! - API, discovery, and runner adapters together;
//! - normal trait calls becoming Prometheus samples;
//! - labels supplied through source-crate contracts;
//! - millisecond durations exported in seconds;
//! - selected exposition lines without histogram bucket noise.
//!
//! Run with
//! `cargo run -p solti-prometheus --example adapter_metrics --features api,discover,runner`.

use prometheus::{Encoder, TextEncoder};
use solti_api::{ApiMetricsBackend, Transport};
use solti_discover::{DiscoverFailReason, DiscoverMetricsBackend};
use solti_prometheus::{
    PrometheusApiMetrics, PrometheusDiscoverMetrics, PrometheusRunnerMetrics, Registry,
    register_build_info,
};
use solti_runner::{MetricsBackend, RunnerErrorKind, RunnerType};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-prometheus: metrics adapters

  solti-api ────────► ApiMetricsBackend ────────► PrometheusApiMetrics ───────┐
  solti-discover ───► DiscoverMetricsBackend ───► PrometheusDiscoverMetrics ──┤
  solti-runner ─────► MetricsBackend ───────────► PrometheusRunnerMetrics ────┤
  build labels ─────► register_build_info() ──────────────────────────────────┤
                                                                              ▼
                                                                       shared Registry
                                                                              │ gather + encode
                                                                              ▼
                                                                     Prometheus exposition text

  Source crates decide when to record an event and which labels it carries.
  This crate owns Prometheus collectors, registration, and unit conversion.
"#;

fn print_metric(exposition: &str, name: &str) {
    let mut found = false;
    for line in exposition.lines().filter(|line| {
        line.strip_prefix(name)
            .is_some_and(|suffix| suffix.starts_with('{') || suffix.starts_with(' '))
    }) {
        println!("      {line}");
        found = true;
    }
    assert!(found, "metric {name} is missing");
}

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Connect several Solti metrics contracts to one scrape-compatible registry."
    );

    let registry = Registry::new();
    register_build_info(&registry, &[("component", "example-agent")])?;
    let api = PrometheusApiMetrics::new(&registry)?;
    let discover = PrometheusDiscoverMetrics::new(&registry)?;
    let runner = PrometheusRunnerMetrics::new(&registry)?;
    println!("[setup] Registered build, API, discovery, and runner collector groups.");

    api.record_in_flight_delta(Transport::Http, 1);
    api.record_request(
        Transport::Http,
        "GET",
        "/apis/solti.io/v1/tasks/{name}",
        200,
        12,
    );
    api.record_in_flight_delta(Transport::Http, -1);
    println!("[api] Recorded one 12 ms HTTP request and returned in-flight requests to zero.");

    discover.record_attempt();
    discover.record_success(25);
    discover.record_attempt();
    discover.record_failure(50, DiscoverFailReason::Timeout);
    discover.record_hold(10);
    println!("[discovery] Recorded one success, one timeout, and one 10 second retry hold.");

    runner.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
    println!("[runner] Recorded one subprocess spawn failure.");

    let families = registry.gather();
    let mut buffer = Vec::new();
    TextEncoder::new().encode(&families, &mut buffer)?;
    let exposition = String::from_utf8(buffer)?;

    println!("[exposition/build]");
    print_metric(&exposition, "solti_build_info");
    println!("[exposition/api]");
    print_metric(&exposition, "solti_api_requests_total");
    print_metric(&exposition, "solti_api_request_duration_seconds_sum");
    print_metric(&exposition, "solti_api_in_flight_requests");
    println!("[exposition/discovery]");
    print_metric(&exposition, "solti_discover_attempts_total");
    print_metric(&exposition, "solti_discover_outcomes_total");
    print_metric(&exposition, "solti_discover_failures_total");
    print_metric(&exposition, "solti_discover_holds_total");
    print_metric(&exposition, "solti_discover_hold_duration_seconds_sum");
    println!("[exposition/runner]");
    print_metric(&exposition, "solti_runner_errors_total");

    println!(
        "\nResult: independent Solti metrics contracts produced one coherent Prometheus scrape."
    );
    Ok(())
}
