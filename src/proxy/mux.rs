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

/// A mux stream with tokio I/O traits (what the relay code consumes).
pub(crate) type MuxStream = Compat<yamux::Stream>;

/// The negotiated ALPN protocol of a TLS stream, if any.
pub(crate) fn negotiated_alpn(stream: &tokio_rustls::TlsStream<TcpStream>) -> Option<Vec<u8>> {
    stream.get_ref().1.alpn_protocol().map(|p| p.to_vec())
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
    /// Identity the peer presented at tunnel establishment; policy is
    /// evaluated against this for every stream (B5: never fabricated).
    pub(crate) peer_identity: SpiffeId,
}

impl Tunnel {
    /// Open a new stream on this tunnel. `Err` means the tunnel is dead and
    /// must be evicted.
    pub(crate) async fn open_stream(&self) -> Result<MuxStream, std::io::Error> {
        let (tx, rx) = oneshot::channel();
        self.open_tx
            .send(tx)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "tunnel closed"))?;
        match rx.await {
            Ok(Ok(stream)) => Ok(stream.compat()),
            Ok(Err(e)) => Err(std::io::Error::other(e)),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "tunnel driver exited",
            )),
        }
    }
}

/// Client-side pool: at most one live tunnel per upstream address.
pub(crate) struct MuxPool {
    tunnels: DashMap<SocketAddr, Tunnel>,
    /// Per-address creation locks (single-flight tunnel establishment).
    creating: DashMap<SocketAddr, Arc<tokio::sync::Mutex<()>>>,
}

impl MuxPool {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            tunnels: DashMap::new(),
            creating: DashMap::new(),
        })
    }

    pub(crate) fn get(&self, addr: &SocketAddr) -> Option<Tunnel> {
        self.tunnels.get(addr).map(|t| t.clone())
    }

    /// Remove the tunnel at `addr` only if it is still `tunnel` (by id).
    pub(crate) fn evict(&self, addr: &SocketAddr, tunnel: &Tunnel) {
        self.tunnels
            .remove_if(addr, |_, current| current.id == tunnel.id);
    }

    /// The per-address creation lock. Hold it across establish-and-register so
    /// concurrent first connections to a peer share one handshake.
    pub(crate) fn creation_lock(&self, addr: SocketAddr) -> Arc<tokio::sync::Mutex<()>> {
        self.creating
            .entry(addr)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .value()
            .clone()
    }

    /// Register a freshly established client session and spawn its driver.
    /// The driver owns the yamux connection: polling it moves all tunnel I/O,
    /// and its exit (peer closed, I/O error) evicts the tunnel from the pool.
    pub(crate) fn register(
        self: &Arc<Self>,
        addr: SocketAddr,
        tls: tokio_rustls::TlsStream<TcpStream>,
        peer_identity: SpiffeId,
    ) -> Tunnel {
        let conn = Connection::new(tls.compat(), YamuxConfig::default(), Mode::Client);
        let (open_tx, open_rx) = mpsc::channel::<OpenRequest>(64);
        let tunnel = Tunnel {
            id: TUNNEL_IDS.fetch_add(1, Ordering::Relaxed),
            open_tx,
            peer_identity,
        };
        self.tunnels.insert(addr, tunnel.clone());
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
    peer: SpiffeId,
) {
    let mut conn = Connection::new(tls.compat(), YamuxConfig::default(), Mode::Server);
    info!("mux tunnel established from {} → {}", peer, upstream);

    loop {
        let next =
            futures_util::future::poll_fn(|cx| conn.poll_next_inbound(cx)).await;
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

async fn relay_stream_to_upstream(mut stream: MuxStream, upstream: SocketAddr) {
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
        warn!("failed to configure mux upstream socket {}: {}", upstream, e);
    }

    match crate::proxy::copy_bidirectional(&mut stream, &mut upstream_stream).await {
        Ok((up, down)) => crate::metrics::record_connection(up, down, start.elapsed()),
        Err(e) => {
            debug!("mux stream relay error → {}: {}", upstream, e);
            crate::metrics::record_connection_failed();
        }
    }
}
