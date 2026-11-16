// B8: No panic paths in connection-handling code.
// Test code is exempt via cfg(test).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub mod handshake;
pub(crate) mod mux;
pub(crate) mod original_dst;
pub mod outbound;
pub mod reload;
pub mod tcp;
pub(crate) mod verify;

pub use handshake::{TlsClient, TlsHandshake, TlsServer};
pub(crate) use original_dst::get_original_dst;
pub use outbound::OutboundProxy;
pub use reload::{TlsCredentialPaths, TlsCredentialReloader};
pub use tcp::TcpProxy;

use std::fmt;
use std::future::{poll_fn, Future};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{ready, Context, Poll};
use std::time::{Duration, Instant};
use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// Injectable upstream I/O (B11); deadlines apply to production TCP dials.
#[async_trait::async_trait]
pub(crate) trait UpstreamDialer: Send + Sync {
    async fn connect(&self, addr: SocketAddr) -> std::io::Result<TcpStream>;
}

struct TcpDialer;

#[async_trait::async_trait]
impl UpstreamDialer for TcpDialer {
    async fn connect(&self, addr: SocketAddr) -> std::io::Result<TcpStream> {
        tcp_connect(TcpStream::connect(addr)).await
    }
}

async fn tcp_connect(
    connect: impl Future<Output = std::io::Result<TcpStream>>,
) -> std::io::Result<TcpStream> {
    tokio::time::timeout(crate::common::constants::timeouts::TCP_CONNECT, connect)
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upstream TCP connect timed out",
            )
        })?
}

pub(crate) trait OriginalDestination: Send + Sync {
    fn get(&self, stream: &TcpStream) -> Option<SocketAddr>;
}

struct NetfilterDestination;

impl OriginalDestination for NetfilterDestination {
    fn get(&self, stream: &TcpStream) -> Option<SocketAddr> {
        get_original_dst(stream)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum UpstreamTarget {
    Socket(SocketAddr),
    /// Keep hostname:port intact so discovery never fabricates a service port.
    Host(Arc<str>),
}

impl UpstreamTarget {
    pub(crate) fn from_config(value: Option<String>) -> Option<Self> {
        let value = value.filter(|value| !value.is_empty())?;
        if let Ok(addr) = value.parse() {
            return Some(Self::Socket(addr));
        }

        Some(Self::Host(Arc::from(value)))
    }
}

impl fmt::Display for UpstreamTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(addr) => addr.fmt(f),
            Self::Host(name) => name.fmt(f),
        }
    }
}

/// Buffer size for bidirectional data copy.
///
/// Default 64 KiB, decided by the R30 interleaved bulk-profile A/B
/// (256 KB payloads: p50 −15 %, proxy CPU −22 % vs 8 KiB; no measurable
/// effect on 1 KB-payload workloads, whose requests never fill either
/// buffer). Overridable via `INTERLINK_COPY_BUF_SIZE` (4 KiB..=1 MiB).
pub(crate) fn copy_buf_size() -> usize {
    static SIZE: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        std::env::var("INTERLINK_COPY_BUF_SIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_COPY_BUF_SIZE)
            .clamp(4096, 1 << 20)
    });
    *SIZE
}

pub(crate) const DEFAULT_COPY_BUF_SIZE: usize = 65536;

/// Bidirectional copy with pooled, un-zeroed `copy_buf_size()` buffers.
///
/// A faithful port of tokio's `copy_bidirectional_with_sizes` state machine:
/// the same half-close propagation (C7 — when one side reaches EOF the other
/// side is shut down: FIN / TLS close_notify; `tests/halfclose_propagation.rs`
/// drives this), the same flush-before-Pending and flush-before-finish
/// ordering, and the same write-top-up behavior. One difference, on purpose:
/// the two per-connection buffers come from a bounded pool of `BytesMut` and
/// are never zeroed — tokio's `CopyBuffer` starts as `vec![0; n]`, costing a
/// full 64 KiB memset plus first-touch page faults on every connection (the
/// round-18 churn RSS-ratchet root cause).
///
/// Buffers return to the pool when the future is dropped — success, error, or
/// cancellation — so every exit recycles (B10).
pub(crate) async fn copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let n = copy_buf_size();
    copy_bidirectional_pooled(a, b, RelayBuffer::new(n), RelayBuffer::new(n)).await
}

/// Free-list of relay buffers. Lifecycle (B10): acquired by a connection,
/// returned on drop, reused after `clear()`; over-capacity and stale-sized
/// buffers are dropped. 128 × 64 KiB = 8 MiB retained ceiling — connections
/// beyond the pool still get un-zeroed `BytesMut` (the memset elimination
/// holds on every path; the pool additionally avoids allocator churn and
/// first-touch page faults).
const MAX_POOLED_RELAY_BUFFERS: usize = 128;

struct RelayBufferPool {
    available: Mutex<Vec<BytesMut>>,
}

static RELAY_BUFFER_POOL: LazyLock<RelayBufferPool> =
    LazyLock::new(|| RelayBufferPool {
        available: Mutex::new(Vec::new()),
    });

impl RelayBufferPool {
    fn acquire(&self, size: usize) -> BytesMut {
        // Poisoning only follows a panic, which aborts this process; taking
        // the data back is strictly better than failing here (B8).
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while let Some(mut buf) = available.pop() {
            if buf.capacity() >= size {
                buf.clear();
                return buf;
            }
            // Stale size class. The knob is process-constant so this cannot
            // happen within one process; drop and try the next.
        }
        BytesMut::with_capacity(size)
    }

    fn release(&self, buf: &mut BytesMut) {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if available.len() < MAX_POOLED_RELAY_BUFFERS && buf.capacity() >= copy_buf_size() {
            available.push(std::mem::take(buf));
        }
    }
}

/// One direction's copy state — tokio `CopyBuffer` semantics over a pooled
/// `BytesMut` (`len` is the fill level, `pos` the write cursor).
struct RelayBuffer {
    buf: BytesMut,
    pos: usize,
    read_done: bool,
    need_flush: bool,
    amt: u64,
}

impl RelayBuffer {
    fn new(size: usize) -> Self {
        Self {
            buf: RELAY_BUFFER_POOL.acquire(size),
            pos: 0,
            read_done: false,
            need_flush: false,
            amt: 0,
        }
    }

    /// Fill from `reader` directly into the buffer's spare capacity.
    ///
    /// This is tokio's own fill (io/util/read_buf.rs, the `ReadBuf` future's
    /// `poll`) lifted with its invariants: the pointer-stability check and
    /// the `advance_mut` length are upstream's, so the un-zeroed
    /// spare-capacity semantics are the ones tokio maintains. The pointer
    /// check returns an error instead of asserting (B12: panic paths in
    /// connection handling become `Result`s; the move it guards is
    /// impossible with a fixed-capacity `BytesMut`).
    fn poll_fill<R>(&mut self, cx: &mut Context<'_>, reader: Pin<&mut R>) -> Poll<io::Result<()>>
    where
        R: AsyncRead + ?Sized,
    {
        use tokio::io::ReadBuf as IoReadBuf;

        if !self.buf.has_remaining_mut() {
            // Callers only fill while len < capacity; unreachable safety net.
            return Poll::Ready(Ok(()));
        }
        let n = {
            let dst = self.buf.chunk_mut();
            let dst = unsafe { dst.as_uninit_slice_mut() };
            let mut buf = IoReadBuf::uninit(dst);
            let ptr = buf.filled().as_ptr();
            ready!(reader.poll_read(cx, &mut buf)?);

            // Ensure the pointer does not change from under us
            if ptr != buf.filled().as_ptr() {
                return Poll::Ready(Err(io::Error::other("copy buffer moved during read")));
            }
            buf.filled().len()
        };

        // Safety: the number of initialized (and read) bytes is guaranteed by
        // `ReadBuf::filled`'s invariants (upstream comment, io/util/read_buf.rs).
        unsafe {
            self.buf.advance_mut(n);
        }
        self.read_done = n == 0;
        Poll::Ready(Ok(()))
    }

    fn poll_write_buf<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<io::Result<usize>>
    where
        R: AsyncRead + ?Sized,
        W: AsyncWrite + ?Sized,
    {
        match writer
            .as_mut()
            .poll_write(cx, &self.buf[self.pos..self.buf.len()])
        {
            Poll::Pending => {
                // Top up the buffer towards full if we can read a bit more
                // data - this should improve the chances of a large write
                if !self.read_done && self.buf.len() < self.buf.capacity() {
                    ready!(self.poll_fill(cx, reader.as_mut()))?;
                }
                Poll::Pending
            }
            res => res,
        }
    }

    /// Port of tokio `CopyBuffer::poll_copy` (io/util/copy.rs): read into
    /// spare capacity, write out in slices, flush when the writer stalls,
    /// finish by flushing once the reader hit EOF. Task-budget bookkeeping
    /// (tokio's `coop::poll_proceed`) is omitted: the socket-level
    /// `poll_read`/`poll_write` implementations still consume the budget
    /// themselves, so this loop cannot monopolize a worker.
    fn poll_copy<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<io::Result<u64>>
    where
        R: AsyncRead + ?Sized,
        W: AsyncWrite + ?Sized,
    {
        loop {
            // If there is some space left in our buffer, then we try to read some
            // data to continue, thus maximizing the chances of a large write.
            if self.buf.len() < self.buf.capacity() && !self.read_done {
                match self.poll_fill(cx, reader.as_mut()) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => {
                        // Ignore pending reads when our buffer is not empty, because
                        // we can try to write data immediately.
                        if self.pos == self.buf.len() {
                            // Try flushing when the reader has no progress to avoid deadlock
                            // when the reader depends on buffered writer.
                            if self.need_flush {
                                ready!(writer.as_mut().poll_flush(cx))?;
                                self.need_flush = false;
                            }

                            return Poll::Pending;
                        }
                    }
                }
            }

            // If our buffer has some data, let's write it out!
            while self.pos < self.buf.len() {
                let i = ready!(self.poll_write_buf(cx, reader.as_mut(), writer.as_mut()))?;
                if i == 0 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write zero byte into writer",
                    )));
                } else {
                    self.pos += i;
                    self.amt += i as u64;
                    self.need_flush = true;
                }
            }

            debug_assert!(
                self.pos <= self.buf.len(),
                "writer returned length larger than input slice"
            );

            // All data has been written, the buffer can be considered empty again
            self.pos = 0;
            self.buf.clear();

            // If we've written all the data and we've seen EOF, flush out the
            // data and finish the transfer.
            if self.read_done {
                ready!(writer.as_mut().poll_flush(cx))?;
                return Poll::Ready(Ok(self.amt));
            }
        }
    }
}

impl Drop for RelayBuffer {
    fn drop(&mut self) {
        RELAY_BUFFER_POOL.release(&mut self.buf);
    }
}

enum TransferState {
    Running(RelayBuffer),
    ShuttingDown(u64),
    Done(u64),
}

fn transfer_one_direction<A, B>(
    cx: &mut Context<'_>,
    state: &mut TransferState,
    r: &mut A,
    w: &mut B,
) -> Poll<io::Result<u64>>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let mut r = Pin::new(r);
    let mut w = Pin::new(w);

    loop {
        match state {
            TransferState::Running(buf) => {
                let count = ready!(buf.poll_copy(cx, r.as_mut(), w.as_mut()))?;
                *state = TransferState::ShuttingDown(count);
            }
            TransferState::ShuttingDown(count) => {
                ready!(w.as_mut().poll_shutdown(cx))?;

                *state = TransferState::Done(*count);
            }
            TransferState::Done(count) => return Poll::Ready(Ok(*count)),
        }
    }
}

async fn copy_bidirectional_pooled<A, B>(
    a: &mut A,
    b: &mut B,
    a_to_b_buffer: RelayBuffer,
    b_to_a_buffer: RelayBuffer,
) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let mut a_to_b = TransferState::Running(a_to_b_buffer);
    let mut b_to_a = TransferState::Running(b_to_a_buffer);
    poll_fn(|cx| {
        let a_to_b = transfer_one_direction(cx, &mut a_to_b, a, b)?;
        let b_to_a = transfer_one_direction(cx, &mut b_to_a, b, a)?;

        // It is not a problem if ready! returns early because transfer_one_direction for the
        // other direction will keep returning TransferState::Done(count) in future calls to poll
        let a_to_b = ready!(a_to_b);
        let b_to_a = ready!(b_to_a);

        Poll::Ready(Ok((a_to_b, b_to_a)))
    })
    .await
}

/// Record a relay's metrics without turning failures into zero-valued latency
/// observations. Shared by inbound, outbound, and mux relays (C2/C3).
pub(crate) fn record_relay_result(
    result: std::io::Result<(u64, u64)>,
    start: Instant,
) -> std::io::Result<(u64, u64)> {
    match &result {
        Ok((bytes_up, bytes_down)) => {
            crate::metrics::record_connection(*bytes_up, *bytes_down, start.elapsed());
        }
        Err(_) => crate::metrics::record_connection_failed(),
    }
    result
}

/// Bind a SO_REUSEPORT listener for one acceptor task.
///
/// Fallible by design (B15): bind and listen fail in ordinary circumstances
/// (port taken by a non-reuseport socket, permission denied on privileged
/// ports) — callers log and count failures; they must not panic (B8).
pub(crate) fn bind_reuseport(
    addr: std::net::SocketAddr,
) -> std::io::Result<tokio::net::TcpListener> {
    let domain = if addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let sock = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&socket2::SockAddr::from(addr))?;
    sock.listen(1024)?;
    let std_listener: std::net::TcpListener = sock.into();
    tokio::net::TcpListener::from_std(std_listener)
}

/// Number of SO_REUSEPORT acceptor tasks per proxy listener.
///
/// Overridable via `INTERLINK_ACCEPTORS` (clamped to 1..=16) so deployments —
/// and A/B benchmarks — can tune or disable multi-acceptor without a rebuild.
pub(crate) fn num_acceptors() -> usize {
    if let Some(n) = std::env::var("INTERLINK_ACCEPTORS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        return n.clamp(1, 16);
    }
    std::cmp::min(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2),
        4,
    )
    .max(2)
}

/// Wait for the shutdown signal on a proxy's watch channel.
/// `None` (no channel installed) never resolves — the acceptor runs forever.
pub(crate) async fn wait_shutdown(shutdown: Option<tokio::sync::watch::Receiver<bool>>) {
    match shutdown {
        Some(mut rx) => {
            // Already-signalled channels must resolve immediately.
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    // Sender dropped: treat as shutdown.
                    return;
                }
            }
        }
        None => std::future::pending().await,
    }
}

/// Apply standard TCP tuning to a proxy socket.
///
/// - `TCP_NODELAY` reduces latency for small TLS records.
/// - TCP keepalive helps detect dead peers.
pub(crate) fn configure_socket(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let sock_ref = socket2::SockRef::from(stream);
    sock_ref.set_keepalive(true)?;
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(300));
    sock_ref.set_tcp_keepalive(&keepalive)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::common::error::InterlinkError;
    use crate::common::identity::SpiffeId;
    use crate::proxy::handshake::{TlsHandshake, TlsStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    pub(crate) const WAIT: Duration = Duration::from_secs(3);

    pub(crate) fn config() -> config::ProxyConfig {
        config::ProxyConfig {
            identity: Some("spiffe://test.local/ns/default/sa/proxy".into()),
            max_connections: Some(4),
            ..Default::default()
        }
    }

    pub(crate) struct ExpectedEndpoint;

    #[async_trait::async_trait]
    impl crate::discovery::dns::DnsResolver for ExpectedEndpoint {
        async fn lookup(&self, name: &str) -> Result<Vec<SocketAddr>, InterlinkError> {
            assert_eq!(name, "backend.test:8443");
            Ok(vec!["192.0.2.7:8443".parse().unwrap()])
        }
    }

    pub(crate) struct DirectDestination;

    impl OriginalDestination for DirectDestination {
        fn get(&self, stream: &TcpStream) -> Option<SocketAddr> {
            // Model non-redirected SO_ORIGINAL_DST on every test platform.
            stream.local_addr().ok()
        }
    }

    pub(crate) struct GateDialer {
        pub calls: AtomicUsize,
        pub release: Semaphore,
    }

    impl GateDialer {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                release: Semaphore::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl UpstreamDialer for GateDialer {
        async fn connect(&self, addr: SocketAddr) -> std::io::Result<TcpStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _gate = self.release.acquire().await.unwrap();
            TcpDialer.connect(addr).await
        }
    }

    pub(crate) struct GateHandshake {
        pub calls: AtomicUsize,
        pub release: Semaphore,
        acceptor: Option<tokio_rustls::TlsAcceptor>,
    }

    #[async_trait::async_trait]
    impl TlsHandshake for GateHandshake {
        async fn connect(&self, _addr: SocketAddr) -> Result<TlsStream, InterlinkError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _gate = self.release.acquire().await.unwrap();
            Err(InterlinkError::Protocol(
                "injected handshake failure".into(),
            ))
        }

        async fn accept(&self, stream: TcpStream) -> Result<TlsStream, InterlinkError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _gate = self.release.acquire().await.unwrap();
            let Some(acceptor) = &self.acceptor else {
                return Err(InterlinkError::Protocol(
                    "injected handshake failure".into(),
                ));
            };
            Ok(TlsStream {
                inner: acceptor.accept(stream).await?.into(),
                peer_identity: Arc::new(SpiffeId::from_uri(
                    "spiffe://test.local/ns/default/sa/client",
                )?),
            })
        }
    }

    pub(crate) fn handshake(success: bool) -> (Arc<GateHandshake>, Arc<rustls::ClientConfig>) {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let certified =
            interlink_test_support::rcgen::generate_simple_self_signed(vec!["localhost".into()])
                .unwrap();
        let cert = certified.cert.der().clone();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let client = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert],
                rustls::pki_types::PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der())
                    .into(),
            )
            .unwrap();
        (
            Arc::new(GateHandshake {
                calls: AtomicUsize::new(0),
                release: Semaphore::new(0),
                acceptor: success.then(|| tokio_rustls::TlsAcceptor::from(Arc::new(server))),
            }),
            Arc::new(client),
        )
    }

    pub(crate) async fn pair() -> (TcpStream, TcpStream, SocketAddr) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpDialer
            .connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, peer) = listener.accept().await.unwrap();
        (client, server, peer)
    }

    pub(crate) async fn wait_for(mut ready: impl FnMut() -> bool) {
        tokio::time::timeout(WAIT, async {
            while !ready() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_metrics_distinguish_success_from_failure() {
        // INTERLINK_METRICS binds handles on first use. Other proxy tests can
        // initialize it without a recorder; isolate this recorder assertion.
        const CHILD: &str = "INTERLINK_RELAY_METRICS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proxy::tests::relay_metrics_distinguish_success_from_failure",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::record_connection_start();
            assert_eq!(
                record_relay_result(Ok((7, 11)), Instant::now()).unwrap(),
                (7, 11)
            );

            crate::metrics::record_connection_start();
            let error = record_relay_result(
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected relay reset",
                )),
                Instant::now(),
            )
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        });

        let rendered = handle.render();
        for expected in [
            "interlink_connections_total 2",
            "interlink_connection_errors_total 1",
            "interlink_bytes_total 18",
            "interlink_connection_duration_seconds_count 1",
        ] {
            assert!(
                rendered.lines().any(|line| line == expected),
                "missing `{expected}` in:\n{rendered}"
            );
        }
    }

    #[test]
    fn relay_buffer_pool_recycles_within_its_cap() {
        // Isolated pool instance: the global free-list is shared with every
        // concurrently-running test, so exact assertions need their own.
        let pool = RelayBufferPool {
            available: Mutex::new(Vec::new()),
        };
        fn len(pool: &RelayBufferPool) -> usize {
            pool.available
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
        }
        let size = copy_buf_size();
        assert_eq!(len(&pool), 0);

        // Releasing beyond the cap retains at most MAX_POOLED_RELAY_BUFFERS.
        let mut held: Vec<BytesMut> = (0..MAX_POOLED_RELAY_BUFFERS + 2)
            .map(|_| pool.acquire(size))
            .collect();
        for buf in held.iter_mut() {
            pool.release(buf);
        }
        assert_eq!(len(&pool), MAX_POOLED_RELAY_BUFFERS);

        // A recycled buffer comes back cleared, in the same size class.
        let recycled = pool.acquire(size);
        assert_eq!(len(&pool), MAX_POOLED_RELAY_BUFFERS - 1);
        assert!(recycled.is_empty());
        assert!(recycled.capacity() >= size);

        // Stale size classes are dropped, not pooled (B10: every state has
        // an exit — the wrong-class buffer is deallocated, not retained).
        let mut stale = BytesMut::with_capacity(64);
        pool.release(&mut stale);
        assert_eq!(len(&pool), MAX_POOLED_RELAY_BUFFERS - 1);

        // A stale-class buffer in the stack is skipped and dropped on
        // acquire, yielding a fresh correctly-sized buffer.
        pool.available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(BytesMut::with_capacity(64));
        assert_eq!(len(&pool), MAX_POOLED_RELAY_BUFFERS);
        let fresh = pool.acquire(size);
        // The stale top-of-stack entry was skipped and dropped; a real
        // buffer came off the stack behind it (127 real - 1 popped).
        assert_eq!(len(&pool), MAX_POOLED_RELAY_BUFFERS - 2);
        assert!(fresh.capacity() >= size);
    }

    #[tokio::test]
    async fn copy_bidirectional_round_trips_multi_buffer_payload() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 16-byte duplex halves force short reads and writes, driving the
        // fill, partial-drain, top-up, and flush paths of the relay loop.
        //
        // Payload bound (validated against stock tokio, R31): a strict echo
        // with a write-then-read client completes only while in-flight
        // bytes fit both copy buffers plus the pipe buffers (~133 KiB at
        // the 64 KiB default; stock tokio's 8 KiB buffers deadlock this
        // exact topology at 32 KiB). 96 KiB spans multiple buffer cycles
        // with margin — 300 KiB deadlocks by construction and is not a
        // valid payload for this topology.
        assert_eq!(
            copy_buf_size(),
            DEFAULT_COPY_BUF_SIZE,
            "test assumes the default copy buffer size"
        );
        let (mut relay_a, echo_side) = tokio::io::duplex(16);
        let (mut relay_b, mut client) = tokio::io::duplex(16);

        let echo = tokio::spawn(async move {
            let mut echo_side = echo_side;
            let mut buf = vec![0u8; 4096];
            loop {
                let n = match echo_side.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                if echo_side.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        });

        const TOTAL: usize = 96 * 1024;
        let payload: Vec<u8> = (0..TOTAL).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();
        let writer = tokio::spawn(async move {
            client.write_all(&payload).await?;
            client.shutdown().await?;
            let mut got = Vec::with_capacity(TOTAL);
            client.read_to_end(&mut got).await?;
            Ok::<_, std::io::Error>(got)
        });

        // The echo topology deadlocks by construction past the absorption
        // bound, so every stage is deadline-bounded: a stall must fail the
        // test, not hang it (A2/A6).
        let (a_to_b, b_to_a, got) = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            async {
                let (a_to_b, b_to_a) =
                    copy_bidirectional(&mut relay_a, &mut relay_b).await?;
                let got = writer.await.unwrap()?;
                echo.await.unwrap();
                Ok::<_, std::io::Error>((a_to_b, b_to_a, got))
            },
        )
        .await
        .expect("relay stalled: payload over the absorption bound, or a copy-loop regression")
        .unwrap();

        // Byte counts are the loop's own accounting, not a proxy metric.
        assert_eq!(a_to_b, TOTAL as u64);
        assert_eq!(b_to_a, TOTAL as u64);
        assert_eq!(got, expected);
    }
}

#[cfg(test)]
mod connect_tests {
    use super::*;

    #[tokio::test]
    async fn upstream_tcp_connect_deadline_cancels_pending_dial() {
        struct Cancelled(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Cancelled {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cancelled.clone();
        let pending_dial = async move {
            let _guard = Cancelled(flag);
            std::future::pending().await
        };
        let error = tokio::time::timeout(
            crate::common::constants::timeouts::TCP_CONNECT + test_support::WAIT,
            tcp_connect(pending_dial),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
    }
}
