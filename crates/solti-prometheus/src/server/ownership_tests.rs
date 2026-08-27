//! Real HTTP ownership checks, independent of benchmark setup retries.

use std::{
    future::Future,
    io::IoSlice,
    net::SocketAddr,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::serve::Listener;
use prometheus::{
    Counter,
    core::{Collector, Desc},
    proto::MetricFamily,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Notify, oneshot},
    task::JoinHandle,
};

use super::*;

const BOUND: Duration = Duration::from_secs(10);
const MARKER: &str = "ownership_probe_total 1\n";

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, future)
        .await
        .expect("HTTP ownership check exceeded its failure bound")
}

struct CountingCollector {
    metric: Counter,
    gathers: Arc<AtomicUsize>,
}

impl Collector for CountingCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.metric.desc()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        self.gathers.fetch_add(1, Ordering::SeqCst);
        self.metric.collect()
    }
}

fn observed_state() -> (Arc<MetricsServerState>, Arc<AtomicUsize>) {
    let registry = Arc::new(Registry::new());
    let gathers = Arc::new(AtomicUsize::new(0));
    let metric = Counter::new("ownership_probe_total", "HTTP ownership probe").unwrap();
    metric.inc();
    registry
        .register(Box::new(CountingCollector {
            metric,
            gathers: Arc::clone(&gathers),
        }))
        .unwrap();
    let config = MetricsServerConfig::new()
        .try_with_max_concurrent_scrapes(1)
        .unwrap();
    (Arc::new(MetricsServerState::new(registry, config)), gathers)
}

/// Pauses inside a successful write, after delivery but before Hyper can advance
/// its owning buffer. Returning Pending after writing would violate AsyncWrite;
/// this bounded synchronous gate instead models preemption of that worker.
#[derive(Default)]
struct WriteGate {
    entered: Notify,
    released: Mutex<bool>,
    changed: Condvar,
}

impl WriteGate {
    fn hold(&self) {
        self.entered.notify_one();
        let released = self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (released, wait) = self
            .changed
            .wait_timeout_while(released, BOUND, |released| !*released)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            *released && !wait.timed_out(),
            "transport gate was not released"
        );
    }

    fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.changed.notify_all();
    }
}

struct ReleaseOnDrop(Arc<WriteGate>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct GatedStream {
    stream: TcpStream,
    gate: Option<Arc<WriteGate>>,
    written: Vec<u8>,
}

impl GatedStream {
    fn observe_write(&mut self, chunks: &[IoSlice<'_>], mut count: usize) {
        if self.gate.is_none() {
            return;
        }
        for chunk in chunks {
            let take = chunk.len().min(count);
            self.written.extend_from_slice(&chunk[..take]);
            count -= take;
            if count == 0 {
                break;
            }
        }
        assert!(
            self.written.len() <= 64 * 1024,
            "unexpected probe response size"
        );
        let Some(header_end) = self
            .written
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
        else {
            return;
        };
        let header = std::str::from_utf8(&self.written[..header_end]).unwrap();
        let length = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .expect("metrics response has an explicit content length");
        if self.written.len() >= header_end + 4 + length {
            assert!(header.starts_with("HTTP/1.1 200 "));
            assert!(self.written.ends_with(MARKER.as_bytes()));
            self.gate.take().unwrap().hold();
        }
    }
}

impl AsyncRead for GatedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buffer)
    }
}

impl AsyncWrite for GatedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, bytes);
        if let Poll::Ready(Ok(count)) = &result {
            self.observe_write(&[IoSlice::new(bytes)], *count);
        }
        result
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write_vectored(cx, buffers);
        if let Poll::Ready(Ok(count)) = &result {
            self.observe_write(buffers, *count);
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

struct GatedListener {
    listener: TcpListener,
    first: Option<Arc<WriteGate>>,
}

impl Listener for GatedListener {
    type Io = GatedStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, address) = self.listener.accept().await.unwrap();
        (
            GatedStream {
                stream,
                gate: self.first.take(),
                written: Vec::new(),
            },
            address,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

struct HttpServer {
    address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl HttpServer {
    async fn start(state: Arc<MetricsServerState>, gate: Option<Arc<WriteGate>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let shutdown = async move {
                let _ = stopped.await;
            };
            if let Some(gate) = gate {
                axum::serve(
                    GatedListener {
                        listener,
                        first: Some(gate),
                    },
                    metrics_router(state),
                )
                .with_graceful_shutdown(shutdown)
                .await
            } else {
                // The stress path uses the unmodified SDK router and TcpListener.
                axum::serve(listener, metrics_router(state))
                    .with_graceful_shutdown(shutdown)
                    .await
            }
        });
        Self {
            address,
            stop: Some(stop),
            task: Some(task),
        }
    }

    async fn close(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        bounded(self.task.as_mut().unwrap()).await.unwrap().unwrap();
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct HttpClient(BufReader<TcpStream>);

impl HttpClient {
    async fn connect(address: SocketAddr) -> Self {
        Self(BufReader::new(
            bounded(TcpStream::connect(address)).await.unwrap(),
        ))
    }

    /// One HTTP request only. No admission retry, reconnect, or sleep.
    async fn scrape(&mut self) -> (StatusCode, Vec<u8>) {
        bounded(async {
            self.0
                .get_mut()
                .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await
                .unwrap();
            let mut line = String::new();
            self.0.read_line(&mut line).await.unwrap();
            let status = line
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<u16>()
                .unwrap();
            let mut length = None;
            let mut header_bytes = line.len();
            loop {
                line.clear();
                assert_ne!(self.0.read_line(&mut line).await.unwrap(), 0);
                header_bytes += line.len();
                assert!(header_bytes <= 16 * 1024, "oversized HTTP headers");
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let length = length.expect("expected Content-Length response framing");
            assert!(length <= 64 * 1024, "oversized HTTP probe body");
            let mut body = vec![0; length];
            self.0.read_exact(&mut body).await.unwrap();
            (StatusCode::from_u16(status).unwrap(), body)
        })
        .await
    }
}

fn assert_success(response: (StatusCode, Vec<u8>)) {
    assert_eq!(response.0, StatusCode::OK);
    assert!(response.1.ends_with(MARKER.as_bytes()));
}

/// Observe the actual semaphore becoming free, without issuing a recovery HTTP
/// request. This cannot trigger Hyper write-buffer cleanup via another request.
async fn observe_slot_release(state: &MetricsServerState) {
    let permit = bounded(state.gather_slots.acquire()).await.unwrap();
    drop(permit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_read_can_precede_transport_owner_release_over_tcp() {
    let (state, gathers) = observed_state();
    let gate = Arc::new(WriteGate::default());
    let release = ReleaseOnDrop(Arc::clone(&gate));
    let server = HttpServer::start(Arc::clone(&state), Some(Arc::clone(&gate))).await;
    let mut first = HttpClient::connect(server.address).await;
    assert_success(first.scrape().await);
    bounded(gate.entered.notified()).await;

    assert_eq!(gathers.load(Ordering::SeqCst), 1);
    assert_eq!(state.gather_slots.available_permits(), 0);
    let mut second = HttpClient::connect(server.address).await;
    let rejected = second.scrape().await;
    assert_eq!(rejected.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(rejected.1.is_empty());
    assert_eq!(
        gathers.load(Ordering::SeqCst),
        1,
        "rejection must not gather"
    );
    assert_eq!(state.gather_slots.available_permits(), 0);

    drop(release);
    observe_slot_release(&state).await;
    assert_success(second.scrape().await);
    assert_eq!(gathers.load(Ordering::SeqCst), 2);
    observe_slot_release(&state).await;
    assert_success(first.scrape().await);
    assert_eq!(gathers.load(Ordering::SeqCst), 3);
    observe_slot_release(&state).await;
    eprintln!(
        "HTTP ownership trace: full client 200 -> transport owner held/slots=0 -> single 503/no gather -> transport released -> slot acquired without HTTP -> single 200"
    );
    drop((first, second));
    server.close().await;
}

async fn sequential_tcp_scrapes_release_without_recovery_requests() {
    const CYCLES: usize = 8_192;
    let (state, gathers) = observed_state();
    let server = HttpServer::start(Arc::clone(&state), None).await;
    let mut first = HttpClient::connect(server.address).await;
    let mut second = HttpClient::connect(server.address).await;
    let mut successes = 0;
    let mut rejections = 0;
    for _ in 0..CYCLES {
        let before = gathers.load(Ordering::SeqCst);
        let response = first.scrape().await;
        match response.0 {
            StatusCode::OK => {
                assert_success(response);
                assert_eq!(gathers.load(Ordering::SeqCst), before + 1);
                successes += 1;
            }
            StatusCode::SERVICE_UNAVAILABLE => {
                assert!(response.1.is_empty());
                assert_eq!(gathers.load(Ordering::SeqCst), before);
                rejections += 1;
            }
            other => panic!("unexpected single-request outcome: {other}"),
        }
        // Do not retry the failed request. Independently observe real release,
        // then assert the next request succeeds on its very first attempt.
        observe_slot_release(&state).await;
        assert_success(second.scrape().await);
        successes += 1;
    }
    observe_slot_release(&state).await;
    assert_eq!(gathers.load(Ordering::SeqCst), successes);
    assert_eq!(successes + rejections, CYCLES * 2);
    assert_eq!(state.gather_slots.available_permits(), 1);
    eprintln!(
        "HTTP release audit: requests={}, success={successes}, transient_503={rejections}, first_attempt_success_after_observed_release={CYCLES}",
        CYCLES * 2
    );
    drop((first, second));
    server.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn sequential_tcp_release_current_thread() {
    bounded(sequential_tcp_scrapes_release_without_recovery_requests()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_tcp_release_multi_thread() {
    bounded(sequential_tcp_scrapes_release_without_recovery_requests()).await;
}
