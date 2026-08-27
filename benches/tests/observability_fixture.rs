//! Regression coverage for untimed scrape admission and collector handshakes.

#![cfg(feature = "observability")]

use std::{
    any::Any,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::FutureExt;
use solti_benches::fixtures::{current_thread, multi_thread};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, oneshot},
    task::JoinHandle,
};

#[allow(dead_code)]
#[path = "../scenarios/boundary_support/observability.rs"]
mod observability_support;

use observability_support::{scrape, setup_scrape, wait_for_collector_entry};

// A failure bound, not a synchronization delay. In particular, a broken gate
// waiter must fail this test before the helper's 30-second fixture deadline.
const TEST_BOUND: Duration = Duration::from_secs(5);
const EXPOSITION: &str = "solti_bench_local_fixture 1\n";

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct ScriptedServer {
    url: String,
    requests: Arc<AtomicUsize>,
    task: AbortOnDrop,
}

impl ScriptedServer {
    async fn start(statuses: Vec<u16>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("scripted HTTP listener");
        let url = format!("http://{}/metrics", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let task = AbortOnDrop(tokio::spawn(async move {
            for status in statuses {
                let (mut socket, _) = listener.accept().await.expect("scrape connection");
                read_request(&mut socket).await;
                observed.fetch_add(1, Ordering::SeqCst);
                let (reason, body) = match status {
                    200 => ("OK", EXPOSITION),
                    503 => ("Service Unavailable", "controlled capacity rejection\n"),
                    500 => ("Internal Server Error", "controlled setup failure\n"),
                    _ => panic!("unsupported scripted HTTP status {status}"),
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\n\
                     Content-Type: text/plain; version=0.0.4\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("scripted scrape response");
            }
        }));
        Self {
            url,
            requests,
            task,
        }
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    async fn finish(mut self) {
        (&mut self.task.0)
            .await
            .expect("scripted HTTP server completion");
    }

    async fn stop(mut self) {
        self.task.0.abort();
        if let Err(error) = (&mut self.task.0).await {
            assert!(error.is_cancelled(), "scripted HTTP server failed: {error}");
        }
    }
}

async fn read_request(socket: &mut TcpStream) {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 512];
    loop {
        let count = socket.read(&mut chunk).await.expect("scrape request bytes");
        assert_ne!(count, 0, "scrape closed before its HTTP request headers");
        request.extend_from_slice(&chunk[..count]);
        assert!(request.len() <= 16 * 1024, "oversized fixture request");
        if request.windows(4).any(|part| part == b"\r\n\r\n") {
            assert!(request.starts_with(b"GET /metrics HTTP/1.1\r\n"));
            return;
        }
    }
}

fn client() -> reqwest::Client {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .connect_timeout(TEST_BOUND)
        .timeout(TEST_BOUND)
        .build()
        .expect("fixture HTTP client")
}

async fn within_test_bound(future: impl Future<Output = ()>) {
    tokio::time::timeout(TEST_BOUND, future)
        .await
        .expect("observability regression exceeded its 5-second failure bound");
}

fn panic_text(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("fixture assertion must preserve its panic message")
}

async fn setup_retries_only_capacity_rejections() {
    let server = ScriptedServer::start(vec![503, 503, 200]).await;
    setup_scrape(client(), server.url.clone()).await;
    assert_eq!(server.request_count(), 3);
    server.finish().await;
}

async fn setup_failure_reaches_the_gate_waiter() {
    // The second response would let an incorrect retry-on-any-error helper
    // succeed. Correct setup stops after the first 500 and preserves that cause.
    let server = ScriptedServer::start(vec![500, 200]).await;
    let entered = Notify::new();
    let mut first = AbortOnDrop(tokio::spawn(setup_scrape(client(), server.url.clone())));
    let result = AssertUnwindSafe(wait_for_collector_entry(&entered, &mut first.0))
        .catch_unwind()
        .await;
    let panic = result.expect_err("an early setup failure must fail the gate waiter");
    let message = panic_text(panic.as_ref());
    assert!(
        message.contains("500"),
        "lost original setup failure: {message}"
    );
    assert!(first.0.is_finished());
    assert_eq!(server.request_count(), 1);
    server.stop().await;
}

async fn collector_entry_preserves_the_held_request() {
    let entered = Arc::new(Notify::new());
    let first_entered = Arc::clone(&entered);
    let (announced, announcement) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let mut first = AbortOnDrop(tokio::spawn(async move {
        first_entered.notify_one();
        announced.send(()).expect("entry announcement receiver");
        released.await.expect("held request release");
    }));

    // Deliver the notification before registering the waiter. Notify must keep
    // this permit, and the wait helper must leave the held JoinHandle usable.
    announcement.await.expect("collector entry announcement");
    wait_for_collector_entry(&entered, &mut first.0).await;
    assert!(
        !first.0.is_finished(),
        "entry must not complete the held request"
    );
    release.send(()).expect("held request release receiver");
    (&mut first.0).await.expect("held request completion");
}

async fn measured_scrape_does_not_retry_rejection() {
    let server = ScriptedServer::start(vec![503, 200]).await;
    let result = AssertUnwindSafe(scrape(client(), server.url.clone(), None))
        .catch_unwind()
        .await;
    let panic = result.expect_err("a measured scrape must reject 503 without retrying");
    let message = panic_text(panic.as_ref());
    assert!(
        message.contains("503"),
        "unexpected measured scrape failure: {message}"
    );
    assert_eq!(server.request_count(), 1);
    server.stop().await;
}

#[test]
fn setup_retries_only_capacity_rejections_current_thread() {
    current_thread().block_on(within_test_bound(setup_retries_only_capacity_rejections()));
}

#[test]
fn setup_retries_only_capacity_rejections_multi_thread() {
    multi_thread().block_on(within_test_bound(setup_retries_only_capacity_rejections()));
}

#[test]
fn setup_failure_reaches_the_gate_waiter_current_thread() {
    current_thread().block_on(within_test_bound(setup_failure_reaches_the_gate_waiter()));
}

#[test]
fn setup_failure_reaches_the_gate_waiter_multi_thread() {
    multi_thread().block_on(within_test_bound(setup_failure_reaches_the_gate_waiter()));
}

#[test]
fn collector_entry_preserves_the_held_request_current_thread() {
    current_thread().block_on(within_test_bound(
        collector_entry_preserves_the_held_request(),
    ));
}

#[test]
fn collector_entry_preserves_the_held_request_multi_thread() {
    multi_thread().block_on(within_test_bound(
        collector_entry_preserves_the_held_request(),
    ));
}

#[test]
fn measured_scrape_does_not_retry_rejection_current_thread() {
    current_thread().block_on(within_test_bound(measured_scrape_does_not_retry_rejection()));
}

#[test]
fn measured_scrape_does_not_retry_rejection_multi_thread() {
    multi_thread().block_on(within_test_bound(measured_scrape_does_not_retry_rejection()));
}
