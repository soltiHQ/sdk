//! TLS configuration and real loopback exchanges. No external endpoint is contacted.

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use solti_benches::fixtures::RUNTIMES;
use solti_benches::report::{CaseFamily, benchmark_main, print_suite_header, record_case};

#[path = "boundary_support/tls.rs"]
mod tls_support;

use tls_support::{EchoEndpoint, Pki, exchange};

const CONFIG: CaseFamily = CaseFamily::intake(
    "tls/cold/configuration_pair",
    "TLS · LOAD CLIENT AND SERVER CONFIGURATION",
    "configured pair",
    "configured pairs",
    "load in-memory PEM and construct validated client and server rustls configurations",
    "certificate/key generation, source construction, configuration destruction, network I/O",
);
const CONNECT: CaseFamily = CaseFamily::lifecycle(
    "tls/cold/connect_handshake_first_exchange",
    "TLS · NEW CONNECTION AND FIRST EXCHANGE",
    "secured connection",
    "secured connections",
    "TCP connect, full TLS handshake, and one verified 32-byte echo round trip",
    "PKI/configuration/runtime setup, one listener retained across samples, server-first TLS/TCP close and server join; session resumption disabled",
)
.without_lifecycle_interpretation();
const EXCHANGE: CaseFamily = CaseFamily::query(
    "tls/steady/encrypted_exchange",
    "TLS · EXISTING CONNECTION EXCHANGE",
    "round trip",
    "round trips",
    "write one payload and read/verify its complete encrypted loopback echo",
    "PKI/configuration/runtime setup, TCP/TLS handshake, warm-up exchange, connection teardown",
);

fn configuration(c: &mut Criterion) {
    print_suite_header("tls");
    let pki = Pki::generate();
    let mut group = c.benchmark_group(CONFIG.group_id);
    group.throughput(Throughput::Elements(1));
    for (mode, mutual) in [("tls", false), ("mtls", true)] {
        group.bench_function(mode, |b| {
            record_case(CONFIG, mode, None);
            b.iter_custom(|iterations| {
                let mut total = Duration::ZERO;
                for _ in 0..iterations {
                    let (client, server) = pki.sources(mutual);
                    let start = Instant::now();
                    let client = client.into_rustls_config().unwrap();
                    let server = server.into_rustls_config().unwrap();
                    total += start.elapsed();
                    std::hint::black_box((client, server));
                }
                total
            });
        });
    }
    group.finish();
}

fn connections(c: &mut Criterion) {
    let pki = Pki::generate();
    let mut group = c.benchmark_group(CONNECT.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, make_runtime) in &RUNTIMES {
        for (mode, mutual) in [("tls", false), ("mtls", true)] {
            let runtime = make_runtime();
            let (client, server) = pki.configurations(mutual);
            let endpoint = runtime.block_on(EchoEndpoint::bind(client, server, mutual));
            group.bench_function(BenchmarkId::new(runtime_name, mode), |b| {
                record_case(CONNECT, runtime_name, Some(mode.into()));
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let session = endpoint.echo(32, 1);
                            let mut received = [0_u8; 32];
                            let payload = [7_u8; 32];
                            let start = Instant::now();
                            let mut connection = endpoint.connect().await;
                            exchange(&mut connection, &payload, &mut received).await;
                            total += start.elapsed();
                            session.finish(connection).await;
                        }
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

fn exchanges(c: &mut Criterion) {
    let pki = Pki::generate();
    let mut group = c.benchmark_group(EXCHANGE.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, make_runtime) in &RUNTIMES {
        for (mode, mutual) in [("tls", false), ("mtls", true)] {
            for bytes in [1_024, 65_536] {
                let variant = format!("{mode}/{bytes}_bytes");
                let runtime = make_runtime();
                let (client, server) = pki.configurations(mutual);
                let endpoint = runtime.block_on(EchoEndpoint::bind(client, server, mutual));
                let payload = vec![7_u8; bytes];
                group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                    record_case(EXCHANGE, runtime_name, Some(variant.clone()));
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let session = endpoint.echo(bytes, iterations + 1);
                            let mut connection = endpoint.connect().await;
                            let mut received = vec![0_u8; bytes];
                            exchange(&mut connection, &payload, &mut received).await;
                            let mut total = Duration::ZERO;
                            for _ in 0..iterations {
                                let start = Instant::now();
                                exchange(&mut connection, &payload, &mut received).await;
                                total += start.elapsed();
                            }
                            session.finish(connection).await;
                            total
                        })
                    });
                });
            }
        }
    }
    group.finish();
}

criterion_group!(benches, configuration, connections, exchanges);

fn main() {
    benchmark_main("tls", benches);
}
