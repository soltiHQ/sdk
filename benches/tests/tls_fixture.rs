//! Regression coverage for fresh TLS sessions on a retained loopback listener.

#![cfg(feature = "tls")]

use std::net::SocketAddr;

use solti_benches::fixtures::{WAIT_BOUND, current_thread, multi_thread};
use tokio_rustls::rustls;

#[allow(dead_code)]
#[path = "../scenarios/boundary_support/tls.rs"]
mod tls_support;

use tls_support::{ClientConnection, EchoEndpoint, EchoReport, Pki, exchange};

const PAYLOAD_BYTES: usize = 32;
const BATCHES: usize = 2;
const CONNECTIONS_PER_BATCH: usize = 16;
const STEADY_EXCHANGES: u64 = 8;

fn assert_full_handshake(handshake: rustls::HandshakeKind) {
    assert!(
        matches!(
            handshake,
            rustls::HandshakeKind::Full | rustls::HandshakeKind::FullWithHelloRetryRequest
        ),
        "expected a full handshake, got {handshake:?}"
    );
}

fn check_client(connection: &ClientConnection, address: SocketAddr) -> SocketAddr {
    let (tcp, tls) = connection.get_ref();
    assert_eq!(tcp.peer_addr().unwrap(), address);
    assert!(!tls.is_handshaking(), "client handshake is incomplete");
    assert_full_handshake(tls.handshake_kind().expect("client handshake kind"));
    assert!(
        tls.peer_certificates()
            .is_some_and(|certificates| !certificates.is_empty()),
        "client did not receive the server identity"
    );
    tcp.local_addr().unwrap()
}

fn check_server(report: EchoReport, expected_peer: SocketAddr, mutual: bool) {
    assert_eq!(report.peer, expected_peer, "accepted a different TCP peer");
    assert_full_handshake(report.handshake);
    assert_eq!(report.authenticated, mutual);
}

fn payload(sequence: usize) -> [u8; PAYLOAD_BYTES] {
    std::array::from_fn(|index| (sequence as u8).wrapping_mul(17).wrapping_add(index as u8))
}

async fn repeated_lifecycle(mutual: bool) {
    let pki = Pki::generate();
    let (client, server) = pki.configurations(mutual);
    let endpoint = EchoEndpoint::bind(client, server, mutual).await;
    let address = endpoint.address();
    assert!(address.ip().is_loopback());
    assert_ne!(address.port(), 0);

    let mut completed = 0;
    // Reuse the endpoint across separate batches as Criterion does across
    // calibration and samples. Every session still owns a new TCP connection.
    for batch in 0..BATCHES {
        for iteration in 0..CONNECTIONS_PER_BATCH {
            let session = endpoint.echo(PAYLOAD_BYTES, 1);
            let mut connection = endpoint.connect().await;
            let expected_peer = check_client(&connection, address);
            let sent = payload(batch * CONNECTIONS_PER_BATCH + iteration);
            let mut received = [0_u8; PAYLOAD_BYTES];
            exchange(&mut connection, &sent, &mut received).await;
            assert_eq!(received, sent);

            // finish checks TLS EOF and TCP FIN in both directions, drops the
            // connection, and joins the server before the next accept starts.
            let report = session.finish(connection).await;
            check_server(report, expected_peer, mutual);
            assert_eq!(endpoint.address(), address);
            completed += 1;
        }
        assert_eq!(completed, (batch + 1) * CONNECTIONS_PER_BATCH);
    }
    assert_eq!(completed, 32);

    // The steady scenario must keep its one established connection throughout
    // all exchanges, then use the same strict teardown as the cold scenario.
    let session = endpoint.echo(PAYLOAD_BYTES, STEADY_EXCHANGES);
    let mut connection = endpoint.connect().await;
    let expected_peer = check_client(&connection, address);
    for iteration in 0..STEADY_EXCHANGES {
        let sent = payload(completed + iteration as usize);
        let mut received = [0_u8; PAYLOAD_BYTES];
        exchange(&mut connection, &sent, &mut received).await;
        assert_eq!(received, sent);
        assert_eq!(check_client(&connection, address), expected_peer);
    }
    check_server(session.finish(connection).await, expected_peer, mutual);
    assert_eq!(endpoint.address(), address);
}

async fn bounded_lifecycle(mutual: bool) {
    // This bounds the entire regression, not merely each socket operation.
    // On failure EchoSession's drop guard aborts its server task.
    tokio::time::timeout(WAIT_BOUND, repeated_lifecycle(mutual))
        .await
        .expect("TLS fixture regression exceeded its deadline");
}

#[test]
fn tls_reuses_listener_current_thread() {
    current_thread().block_on(bounded_lifecycle(false));
}

#[test]
fn tls_reuses_listener_multi_thread() {
    multi_thread().block_on(bounded_lifecycle(false));
}

#[test]
fn mutual_tls_reuses_listener_current_thread() {
    current_thread().block_on(bounded_lifecycle(true));
}

#[test]
fn mutual_tls_reuses_listener_multi_thread() {
    multi_thread().block_on(bounded_lifecycle(true));
}
