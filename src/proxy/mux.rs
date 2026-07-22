// B8: No panic paths in connection-handling code.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

//! Multiplexed mTLS tunnels between interlink proxies ("handshake avoidance").
//!
//! One mTLS connection per upstream peer carries many application connections
//! as yamux streams, so the full-handshake crypto (x25519 + ed25519 — ~25 % of
//! proxy CPU under churn per the committed flamegraph) is paid once per peer
//! instead of once per connection.
//!
//! Negotiated via ALPN `il/mux/1` (C7: the decision point is the TLS
//! handshake's ALPN extension — both sides know the mode before any
//! application byte flows). Peers that don't offer it fall back to the legacy
//! 1:1 relay, so mixed proxy versions interoperate.
//!
//! Stream addressing: a tunnel is keyed by the original destination
//! (`ip:port`) exactly like a legacy connection, and the receiving inbound
//! proxy routes *every* stream in the tunnel to the upstream it recovered for
//! the tunnel's TCP connection (SO_ORIGINAL_DST / default_upstream). Streams
//! therefore need no per-stream address header.
//!
//! Multiplexer: the libp2p-maintained `yamux` crate. (`tokio-yamux` was tried
//! first and rejected by a size-sweep probe: it deadlocks flow control at
//! exactly >256 KiB per stream even with an actively reading peer — B14's
//! "test the documented semantics" check.) The crate is futures-io based;
//! `tokio_util::compat` bridges to tokio traits at both edges.
//!
//! Lifecycle (B10):
//! | state | reader/opener action | exit |
//! |---|---|---|
//! | no tunnel for addr | TLS connect; if ALPN=mux, register tunnel | tunnel live, or legacy fallback |
//! | tunnel live | `open_stream()` — no handshake | stream relayed |
//! | tunnel dead (driver exited / open fails) | evict from pool, re-establish | fresh tunnel or legacy |
//!
//! Creation is single-flight per address (the DNS-stampede lesson): concurrent
//! connections to a new peer wait on a per-address async mutex instead of each
//! opening its own tunnel.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::Poll;

use dashmap::DashMap;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::{debug, info, warn};
use yamux::{Config as YamuxConfig, Connection, Mode};

use crate::common::identity::SpiffeId;

/// ALPN protocol id for multiplexed proxy-to-proxy tunnels.
pub(crate) const ALPN_MUX: &[u8] = b"il/mux/1";

/// A mux stream with tokio I/O traits (what the relay code consumes),
/// carrying a guard that decrements its tunnel's live-stream counter on drop
/// so the pool can load-balance and grow. `yamux::Stream` is `Unpin`, so the
/// wrapper delegates without pin projection.
pub(crate) struct MuxStream {
    inner: Compat<yamux::Stream>,
    _guard: LiveGuard,
}

struct LiveGuard(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

impl tokio::io::AsyncRead for MuxStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for MuxStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Soft cap on live streams per tunnel. Kept well under yamux's hard
/// `max_num_streams` (512) and its ack-backlog (256): once a tunnel reaches
/// this, the pool opens another tunnel to the peer rather than overloading one
/// session (which is what capped concurrency at 512 and dropped connections).
const STREAMS_PER_TUNNEL: usize = 200;

/// Maximum tunnels the pool will open to a single peer. 16 × 200 = 3200 live
/// streams per peer before back-pressure, comfortably above the heavy profile.
const MAX_TUNNELS_PER_PEER: usize = 16;

/// The negotiated ALPN protocol of a TLS stream, if any.
pub(crate) fn negotiated_alpn(stream: &tokio_rustls::TlsStream<TcpStream>) -> Option<&[u8]> {
    stream.get_ref().1.alpn_protocol()
}

static TUNNEL_IDS: AtomicU64 = AtomicU64::new(0);

type OpenRequest = oneshot::Sender<Result<yamux::Stream, yamux::ConnectionError>>;

/// A live client-side tunnel to one upstream peer.
#[derive(Clone)]
pub(crate) struct Tunnel {
    /// Unique id so eviction can be compare-and-remove: a driver exiting for
    /// an old tunnel must not evict a newer replacement at the same address.
    id: u64,
    open_tx: mpsc::Sender<OpenRequest>,
    /// Live streams currently open on this tunnel (for pool load-balancing).
    live: Arc<std::sync::atomic::AtomicUsize>,
    /// Identity the peer presented at tunnel establishment; policy is
    /// evaluated against this for every stream (B5: never fabricated).
    pub(crate) peer_identity: Arc<SpiffeId>,
}

impl Tunnel {
    /// Streams currently live on this tunnel.
    pub(crate) fn load(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// Open a new stream on this tunnel. `Err` means the tunnel is dead and
    /// must be evicted. The returned stream decrements `live` on drop.
    pub(crate) async fn open_stream(&self) -> Result<MuxStream, std::io::Error> {
        // Reserve the slot before opening so concurrent pickers see the load.
        self.live.fetch_add(1, Ordering::AcqRel);
        let guard = LiveGuard(self.live.clone());
        let (tx, rx) = oneshot::channel();
        self.open_tx
            .send(tx)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "tunnel closed"))?;
        match rx.await {
            Ok(Ok(stream)) => Ok(MuxStream {
                inner: stream.compat(),
                _guard: guard,
            }),
            Ok(Err(e)) => Err(std::io::Error::other(e)),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "tunnel driver exited",
            )),
        }
        // On Err, `guard` drops here and releases the reserved slot.
    }
}

/// Client-side pool: a set of tunnels per upstream peer. Streams are spread
/// across tunnels (each capped at `STREAMS_PER_TUNNEL`) so no single yamux
/// session becomes a concurrency ceiling or a single point of failure.
pub(crate) struct MuxPool {
    peers: DashMap<SocketAddr, Vec<Tunnel>>,
    /// Per-address creation locks (single-flight tunnel establishment).
    creating: DashMap<SocketAddr, Arc<tokio::sync::Mutex<()>>>,
}

impl MuxPool {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            peers: DashMap::new(),
            creating: DashMap::new(),
        })
    }

    /// Least-loaded tunnel for the peer whose load is under the soft cap.
    /// `None` means every tunnel is at capacity (or there are none) — the
    /// caller should grow the pool.
    pub(crate) fn pick_under_cap(&self, addr: &SocketAddr) -> Option<Tunnel> {
        let tunnels = self.peers.get(addr)?;
        tunnels
            .iter()
            .filter(|t| t.load() < STREAMS_PER_TUNNEL)
            .min_by_key(|t| t.load())
            .cloned()
    }

    /// Least-loaded tunnel for the peer regardless of cap (used when the pool
    /// is already at `MAX_TUNNELS_PER_PEER` and must not grow further).
    pub(crate) fn pick_any(&self, addr: &SocketAddr) -> Option<Tunnel> {
        let tunnels = self.peers.get(addr)?;
        tunnels.iter().min_by_key(|t| t.load()).cloned()
    }

    /// May the pool open another tunnel to this peer?
    pub(crate) fn can_grow(&self, addr: &SocketAddr) -> bool {
        self.peers.get(addr).map(|t| t.len()).unwrap_or(0) < MAX_TUNNELS_PER_PEER
    }

    /// Remove a specific tunnel (by id) from the peer's pool.
    pub(crate) fn evict(&self, addr: &SocketAddr, tunnel: &Tunnel) {
        if let Some(mut tunnels) = self.peers.get_mut(addr) {
            tunnels.retain(|t| t.id != tunnel.id);
        }
    }

    /// The per-address creation lock. Held only while establishing a new
    /// tunnel so concurrent first connections to a peer don't each open one.
    pub(crate) fn creation_lock(&self, addr: SocketAddr) -> Arc<tokio::sync::Mutex<()>> {
        self.creating
            .entry(addr)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .value()
            .clone()
    }

    /// Register a freshly established client session, add it to the peer's
    /// pool, and spawn its driver. The driver owns the yamux connection;
    /// its exit (peer closed / I/O error) evicts the tunnel.
    pub(crate) fn register(
        self: &Arc<Self>,
        addr: SocketAddr,
        tls: tokio_rustls::TlsStream<TcpStream>,
        peer_identity: Arc<SpiffeId>,
    ) -> Tunnel {
        let conn = Connection::new(tls.compat(), YamuxConfig::default(), Mode::Client);
        let (open_tx, open_rx) = mpsc::channel::<OpenRequest>(256);
        let tunnel = Tunnel {
            id: TUNNEL_IDS.fetch_add(1, Ordering::Relaxed),
            open_tx,
            live: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            peer_identity,
        };
        self.peers.entry(addr).or_default().push(tunnel.clone());
        crate::metrics::record_mux_tunnel_opened();

        let pool = self.clone();
        let driver_tunnel = tunnel.clone();
        tokio::spawn(async move {
            drive_client(conn, open_rx).await;
            info!("mux tunnel to {} ended", addr);
            pool.evict(&addr, &driver_tunnel);
        });
        tunnel
    }
}

/// Drive a client-mode yamux connection: serve open requests and pump I/O.
///
/// One future polls both the open-request queue and `poll_next_inbound` (the
/// connection's engine — it must be polled continuously to move any data).
/// At most one open is in flight at a time; its reply is stashed across
/// `Pending` so a request is never dropped.
async fn drive_client(
    mut conn: Connection<Compat<tokio_rustls::TlsStream<TcpStream>>>,
    mut open_rx: mpsc::Receiver<OpenRequest>,
) {
    let mut pending_open: Option<OpenRequest> = None;
    let mut open_queue_closed = false;

    futures_util::future::poll_fn(|cx| {
        // 1. Serve stream-open requests, one at a time.
        loop {
            if pending_open.is_none() && !open_queue_closed {
                match open_rx.poll_recv(cx) {
                    Poll::Ready(Some(req)) => pending_open = Some(req),
                    Poll::Ready(None) => open_queue_closed = true,
                    Poll::Pending => {}
                }
            }
            match pending_open.take() {
                None => break,
                Some(req) => {
                    if req.is_closed() {
                        // Requester gave up (timeout/abort); don't open an
                        // orphan stream on its behalf.
                        continue;
                    }
                    match conn.poll_new_outbound(cx) {
                        Poll::Ready(res) => {
                            let _ = req.send(res);
                        }
                        Poll::Pending => {
                            pending_open = Some(req);
                            break;
                        }
                    }
                }
            }
        }

        // 2. Pump the connection engine; drop unexpected server-initiated
        //    streams (servers must not open streams toward us).
        loop {
            match conn.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(unexpected))) => {
                    warn!(
                        "dropping unexpected server-initiated mux stream {}",
                        unexpected.id()
                    );
                }
                Poll::Ready(Some(Err(e))) => {
                    debug!("mux tunnel error: {}", e);
                    return Poll::Ready(());
                }
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => return Poll::Pending,
            }
        }
    })
    .await;
}

/// Serve the inbound side of a mux tunnel: accept streams and relay each to
/// `upstream`. Returns when the tunnel closes. Each stream is accounted as a
/// connection in metrics; the tunnel itself holds the accept-side permit.
pub(crate) async fn serve_tunnel(
    tls: tokio_rustls::TlsStream<TcpStream>,
    upstream: SocketAddr,
    peer: Arc<SpiffeId>,
) {
    let mut conn = Connection::new(tls.compat(), YamuxConfig::default(), Mode::Server);
    info!("mux tunnel established from {} → {}", peer, upstream);

    loop {
        let next = futures_util::future::poll_fn(|cx| conn.poll_next_inbound(cx)).await;
        let stream = match next {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                debug!("mux tunnel from {} closed: {}", peer, e);
                break;
            }
            None => break,
        };
        crate::metrics::record_mux_stream();
        tokio::spawn(relay_stream_to_upstream(stream.compat(), upstream));
    }
    info!("mux tunnel from {} ended", peer);
}

async fn relay_stream_to_upstream(mut stream: Compat<yamux::Stream>, upstream: SocketAddr) {
    let start = std::time::Instant::now();
    crate::metrics::record_connection_start();

    let mut upstream_stream = match TcpStream::connect(upstream).await {
        Ok(s) => s,
        Err(e) => {
            warn!("mux stream upstream connect failed to {}: {}", upstream, e);
            crate::metrics::record_connection_failed();
            return;
        }
    };
    if let Err(e) = crate::proxy::configure_socket(&upstream_stream) {
        warn!(
            "failed to configure mux upstream socket {}: {}",
            upstream, e
        );
    }

    if let Err(e) = crate::proxy::record_relay_result(
        crate::proxy::copy_bidirectional(&mut stream, &mut upstream_stream).await,
        start,
    ) {
        debug!("mux stream relay error → {}: {}", upstream, e);
    }
}
