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
//! | tunnel idle (no live streams, no opens for 60s) | driver exits and evicts peer entry | no tunnel |
//! | creation in flight | callers share one mutex | final holder removes entry, even on cancellation |
//! | inbound tunnel reaches max age | reset new streams, pump existing relays | GOAWAY after drain |
//!
//! Creation is single-flight per address (the DNS-stampede lesson): concurrent
//! connections to a new peer wait on a per-address async mutex instead of each
//! opening its own tunnel.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use dashmap::DashMap;
use futures_util::{stream::FuturesUnordered, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::{debug, info, warn};
use yamux::{Config as YamuxConfig, Connection, Mode};

use crate::common::identity::SpiffeId;
use crate::common::{constants::mux, error::InterlinkError};
use crate::policy::{Decision, PolicyEngine};

/// ALPN protocol id for multiplexed proxy-to-proxy tunnels.
pub(crate) const ALPN_MUX: &[u8] = b"il/mux/1";

/// A mux stream with tokio I/O traits (what the relay code consumes),
/// carrying a guard that decrements its tunnel's live-stream counter on drop
/// so the pool can load-balance and grow. `yamux::Stream` is `Unpin`, so the
/// wrapper delegates without pin projection.
pub(crate) struct MuxStream {
    inner: Compat<yamux::Stream>,
    _guard: LiveGuard,
    _permit: OwnedSemaphorePermit,
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

/// Maximum tunnels per peer: 16 × the configured stream cap (1024 streams
/// at the default 64), independent of outbound's dead-tunnel retry budget.
/// Inbound relays across all peers still share the 4096 global-stream budget.
const MAX_TUNNELS_PER_PEER: usize = 16;

const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const ADMISSION_GRACE: Duration = Duration::from_millis(250);
const MAX_PENDING_ADMISSIONS: usize = 64;

/// Fix each stream's receive credit at STREAM_WINDOW. Yamux reserves the
/// default credit for every possible stream before allocating auto-tune credit;
/// setting the connection budget to exactly that reservation disables growth.
/// One extra protocol slot permits accepting and resetting an over-cap stream
/// instead of triggering yamux's connection-fatal hard stream limit.
fn yamux_config() -> YamuxConfig {
    let hard_cap = mux::TOTAL_STREAMS + 1;
    let mut config = YamuxConfig::default();
    config
        .set_max_connection_receive_window(None)
        .set_max_num_streams(hard_cap)
        .set_max_connection_receive_window(Some(hard_cap * mux::STREAM_WINDOW as usize));
    config
}

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
    slots: Arc<Semaphore>,
    /// Identity the peer presented at tunnel establishment; policy is
    /// evaluated against this for every stream (B5: never fabricated).
    pub(crate) peer_identity: Arc<SpiffeId>,
}

impl Tunnel {
    /// Streams currently live on this tunnel.
    pub(crate) fn load(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// Reserve atomically before a pool picker exposes this tunnel. The
    /// semaphore begins with the configured cap; each lease transfers its
    /// permit into the opened stream, releasing on failure/cancel/stream drop.
    fn try_reserve(&self) -> Option<ReservedTunnel> {
        let permit = self.slots.clone().try_acquire_owned().ok()?;
        self.live.fetch_add(1, Ordering::AcqRel);
        Some(ReservedTunnel {
            tunnel: self.clone(),
            permit,
            guard: LiveGuard(self.live.clone()),
        })
    }
}

/// A tunnel slot already reserved by the pool. Not Clone: exactly one open
/// consumes this lease, so selection and opening cannot oversubscribe a cap.
pub(crate) struct ReservedTunnel {
    tunnel: Tunnel,
    permit: OwnedSemaphorePermit,
    guard: LiveGuard,
}

impl std::ops::Deref for ReservedTunnel {
    type Target = Tunnel;

    fn deref(&self) -> &Self::Target {
        &self.tunnel
    }
}

impl ReservedTunnel {
    pub(crate) async fn open_stream(self) -> Result<MuxStream, std::io::Error> {
        let Self {
            tunnel,
            permit,
            guard,
        } = self;
        let (tx, rx) = oneshot::channel();
        tunnel
            .open_tx
            .send(tx)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "tunnel closed"))?;
        match rx.await {
            Ok(Ok(stream)) => Ok(MuxStream {
                inner: stream.compat(),
                _guard: guard,
                _permit: permit,
            }),
            Ok(Err(e)) => Err(std::io::Error::other(e)),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "tunnel driver exited",
            )),
        }
    }
}

/// Client-side pool: a set of tunnels per upstream peer. Streams are spread
/// across tunnels, each capped by the same configuration value as the inbound
/// stream budget. Both peers should configure matching caps to avoid resets.
pub(crate) struct MuxPool {
    peers: DashMap<SocketAddr, Vec<Tunnel>>,
    /// Per-address creation locks (single-flight tunnel establishment).
    creating: DashMap<SocketAddr, Arc<tokio::sync::Mutex<()>>>,
    stream_cap: usize,
}

/// Last holder removes the election entry, including on cancellation/failure.
/// Lookup and compare-and-remove are both under DashMap's shard lock, so a
/// waiter can never acquire a different mutex while an older one is in use.
pub(crate) struct CreationLock {
    pool: Arc<MuxPool>,
    addr: SocketAddr,
    inner: Arc<tokio::sync::Mutex<()>>,
}

impl std::ops::Deref for CreationLock {
    type Target = tokio::sync::Mutex<()>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Drop for CreationLock {
    fn drop(&mut self) {
        self.pool.creating.remove_if(&self.addr, |_, lock| {
            Arc::ptr_eq(lock, &self.inner) && Arc::strong_count(lock) == 2
        });
    }
}

impl MuxPool {
    pub(crate) fn new(max_streams_per_tunnel: usize) -> Arc<Self> {
        Arc::new(Self {
            peers: DashMap::new(),
            creating: DashMap::new(),
            stream_cap: max_streams_per_tunnel,
        })
    }

    /// Reserve a slot on the least-loaded tunnel. Contending pickers that
    /// lose the semaphore race try again or return None to trigger growth.
    pub(crate) fn pick_under_cap(&self, addr: &SocketAddr) -> Option<ReservedTunnel> {
        let tunnels = self.peers.get(addr)?;
        loop {
            let tunnel = tunnels
                .iter()
                .filter(|t| t.slots.available_permits() > 0 && !t.open_tx.is_closed())
                .min_by_key(|t| t.load())?;
            if let Some(reserved) = tunnel.try_reserve() {
                return Some(reserved);
            }
        }
    }

    /// At the peer's tunnel limit, still never exceed a tunnel's hard budget.
    /// A concurrently released slot may be available after the growth check.
    pub(crate) fn pick_any(&self, addr: &SocketAddr) -> Option<ReservedTunnel> {
        self.pick_under_cap(addr)
    }

    /// May the pool open another tunnel to this peer?
    pub(crate) fn can_grow(&self, addr: &SocketAddr) -> bool {
        self.peers.get(addr).map(|t| t.len()).unwrap_or(0) < MAX_TUNNELS_PER_PEER
    }

    /// Remove a specific tunnel (by id) from the peer's pool.
    pub(crate) fn evict(&self, addr: &SocketAddr, tunnel: &Tunnel) {
        self.peers.remove_if_mut(addr, |_, tunnels| {
            tunnels.retain(|t| t.id != tunnel.id);
            tunnels.is_empty()
        });
        self.sweep_creating();
    }

    /// The per-address creation lock. Held only while establishing a new
    /// tunnel so concurrent first connections to a peer don't each open one.
    pub(crate) fn creation_lock(self: &Arc<Self>, addr: SocketAddr) -> CreationLock {
        self.sweep_creating();
        let inner = self
            .creating
            .entry(addr)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .value()
            .clone();
        CreationLock {
            pool: self.clone(),
            addr,
            inner,
        }
    }

    fn sweep_creating(&self) {
        self.creating.retain(|_, lock| Arc::strong_count(lock) > 1);
    }

    #[cfg(test)]
    fn entry_counts(&self) -> (usize, usize) {
        (self.peers.len(), self.creating.len())
    }

    /// Register a freshly established client session, add it to the peer's
    /// pool, and spawn its driver. The driver owns the yamux connection;
    /// its exit (peer closed / I/O error) evicts the tunnel.
    pub(crate) fn register(
        self: &Arc<Self>,
        addr: SocketAddr,
        tls: tokio_rustls::TlsStream<TcpStream>,
        peer_identity: Arc<SpiffeId>,
    ) -> Result<ReservedTunnel, std::io::Error> {
        self.register_connection(addr, tls, peer_identity)
    }

    fn register_connection<T>(
        self: &Arc<Self>,
        addr: SocketAddr,
        io: T,
        peer_identity: Arc<SpiffeId>,
    ) -> Result<ReservedTunnel, std::io::Error>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        if self.stream_cap == 0 || self.stream_cap > Semaphore::MAX_PERMITS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid mux stream cap",
            ));
        }
        let conn = Connection::new(io.compat(), yamux_config(), Mode::Client);
        let (open_tx, open_rx) = mpsc::channel::<OpenRequest>(256);
        let tunnel = Tunnel {
            id: TUNNEL_IDS.fetch_add(1, Ordering::Relaxed),
            open_tx,
            live: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            slots: Arc::new(Semaphore::new(self.stream_cap)),
            peer_identity,
        };
        // Reserve the first opener before publication, so fast-path pickers
        // cannot take its slot while establishment is returning to its caller.
        let first = tunnel.try_reserve().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "new tunnel has no stream slot",
            )
        })?;
        self.peers.entry(addr).or_default().push(tunnel.clone());
        crate::metrics::record_mux_tunnel_opened();

        let pool = self.clone();
        let driver_tunnel = tunnel.clone();
        tokio::spawn(async move {
            drive_client(
                conn,
                open_rx,
                driver_tunnel.live.clone(),
                TUNNEL_IDLE_TIMEOUT,
            )
            .await;
            info!("mux tunnel to {} ended", addr);
            pool.evict(&addr, &driver_tunnel);
        });
        Ok(first)
    }
}

/// Drive a client-mode yamux connection: serve open requests and pump I/O.
///
/// One future polls both the open-request queue and `poll_next_inbound` (the
/// connection's engine — it must be polled continuously to move any data).
/// At most one open is in flight at a time; its reply is stashed across
/// `Pending` so a request is never dropped.
async fn drive_client<T>(
    mut conn: Connection<T>,
    mut open_rx: mpsc::Receiver<OpenRequest>,
    live: Arc<std::sync::atomic::AtomicUsize>,
    idle_timeout: Duration,
) where
    T: futures_util::io::AsyncRead + futures_util::io::AsyncWrite + Unpin,
{
    let mut pending_open: Option<OpenRequest> = None;
    let mut open_queue_closed = false;
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);

    futures_util::future::poll_fn(|cx| {
        use std::future::Future;
        if idle.as_mut().poll(cx).is_ready() {
            if live.load(Ordering::Acquire) == 0 {
                return Poll::Ready(());
            }
            idle.as_mut()
                .reset(tokio::time::Instant::now() + idle_timeout);
        }
        // 1. Serve stream-open requests, one at a time.
        loop {
            if pending_open.is_none() && !open_queue_closed {
                match open_rx.poll_recv(cx) {
                    Poll::Ready(Some(req)) => {
                        idle.as_mut()
                            .reset(tokio::time::Instant::now() + idle_timeout);
                        pending_open = Some(req);
                    }
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

/// Inbound stream budgets are shared across every tunnel on this proxy.
pub(crate) struct TunnelLimits {
    pub max_age: Duration,
    pub max_streams_per_tunnel: usize,
    pub total_streams: Arc<Semaphore>,
}

/// Reauthorize and reserve both stream budgets before spawning a relay.
/// On age expiry, reject new streams while continuing to pump yamux until all
/// existing relays finish, then send GOAWAY via the library's close primitive.
pub(crate) async fn serve_tunnel(
    tls: tokio_rustls::TlsStream<TcpStream>,
    upstream: SocketAddr,
    peer_id: SpiffeId,
    local_id: SpiffeId,
    policy: Arc<PolicyEngine>,
    limits: TunnelLimits,
) -> Result<(), InterlinkError> {
    serve_connection(tls, upstream, peer_id, local_id, policy, limits).await
}

async fn serve_connection<T>(
    io: T,
    upstream: SocketAddr,
    peer_id: SpiffeId,
    local_id: SpiffeId,
    policy: Arc<PolicyEngine>,
    limits: TunnelLimits,
) -> Result<(), InterlinkError>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if limits.max_streams_per_tunnel == 0 || limits.max_streams_per_tunnel > Semaphore::MAX_PERMITS
    {
        return Err(InterlinkError::Config(
            "invalid per-tunnel stream limit".into(),
        ));
    }
    let mut conn = Connection::new(io.compat(), yamux_config(), Mode::Server);
    let per_tunnel = Arc::new(Semaphore::new(limits.max_streams_per_tunnel));
    // Each admitted relay owns one permit from each budget. Both guards are
    // moved into its task and released on completion/cancellation; no waiting
    // relays are spawned when either budget is exhausted.
    let mut relays = tokio::task::JoinSet::new();
    let mut pending = FuturesUnordered::new();
    let pending_cap = MAX_PENDING_ADMISSIONS.min(limits.max_streams_per_tunnel);
    let deadline = tokio::time::sleep(limits.max_age);
    tokio::pin!(deadline);
    let mut draining = false;
    info!(
        "mux tunnel established from {} → {}",
        peer_id.sanitized(),
        upstream
    );
    loop {
        if draining && relays.is_empty() {
            return futures_util::future::poll_fn(|cx| conn.poll_close(cx))
                .await
                .map_err(|e| InterlinkError::Protocol(e.to_string()));
        }
        tokio::select! {
            biased;
            () = &mut deadline, if !draining => {
                draining = true;
                pending.clear();
            }
            Some(result) = relays.join_next(), if !relays.is_empty() => {
                if let Err(e) = result {
                    debug!("mux relay task ended: {}", e);
                }
            }
            Some(admitted) = pending.next(), if !pending.is_empty() => {
                if let Some(admitted) = admitted {
                    spawn_relay(&mut relays, admitted, upstream);
                }
            }
            next = futures_util::future::poll_fn(|cx| conn.poll_next_inbound(cx)) => {
                let stream = match next {
                    Some(Ok(stream)) => stream,
                    Some(Err(e)) => return Err(InterlinkError::Protocol(e.to_string())),
                    None => return Ok(()),
                };
                // Dropping an open yamux stream sends RST (yamux on_drop_stream).
                if draining {
                    drop(stream);
                    continue;
                }
                if let Decision::Deny(reason) = policy.evaluate(&peer_id, &local_id) {
                    warn!("mux stream denied {} → {}: {}", peer_id.sanitized(), local_id.sanitized(), reason);
                    drop(stream);
                    continue;
                }
                // A client's local close can precede completion of the remote
                // relay. Permits gate RELAY SPAWNING, not the already-created
                // stream: briefly queue contention rather than reset a valid
                // replacement while its predecessor's close is in flight.
                // Keep pumping yamux during this wait so that close can arrive.
                match (
                    per_tunnel.clone().try_acquire_owned(),
                    limits.total_streams.clone().try_acquire_owned(),
                ) {
                    (Ok(tunnel_permit), Ok(total_permit)) => {
                        spawn_relay(&mut relays, AdmittedStream {
                            stream,
                            tunnel_permit,
                            total_permit,
                        }, upstream);
                    }
                    _ => {
                        if pending.len() < pending_cap {
                            pending.push(wait_for_admission(
                                stream,
                                per_tunnel.clone(),
                                limits.total_streams.clone(),
                            ));
                        } else {
                            // Bound contention memory too: a hostile peer
                            // cannot grow the grace queue without limit.
                            drop(stream);
                        }
                    }
                }
            }
        }
    }
}

struct AdmittedStream {
    stream: yamux::Stream,
    tunnel_permit: OwnedSemaphorePermit,
    total_permit: OwnedSemaphorePermit,
}

async fn wait_for_admission(
    stream: yamux::Stream,
    per_tunnel: Arc<Semaphore>,
    total_streams: Arc<Semaphore>,
) -> Option<AdmittedStream> {
    // One combined deadline, including both semaphore waits. Cancellation,
    // timeout, or a closed budget drops the stream and every acquired guard.
    let (tunnel_permit, total_permit) = tokio::time::timeout(ADMISSION_GRACE, async {
        let tunnel_permit = per_tunnel.acquire_owned().await.ok()?;
        let total_permit = total_streams.acquire_owned().await.ok()?;
        Some((tunnel_permit, total_permit))
    })
    .await
    .ok()??;
    Some(AdmittedStream {
        stream,
        tunnel_permit,
        total_permit,
    })
}

fn spawn_relay(
    relays: &mut tokio::task::JoinSet<()>,
    admitted: AdmittedStream,
    upstream: SocketAddr,
) {
    crate::metrics::record_mux_stream();
    relays.spawn(async move {
        let _tunnel_permit = admitted.tunnel_permit;
        let _total_permit = admitted.total_permit;
        relay_stream_to_upstream(admitted.stream.compat(), upstream).await;
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::identity::CompiledPattern;
    use crate::policy::PolicyRule;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::timeout;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn identity(account: &str) -> SpiffeId {
        SpiffeId::try_new("example.org", "apps", account).unwrap()
    }

    fn allow_rule() -> PolicyRule {
        PolicyRule {
            source: CompiledPattern::any(),
            destination: CompiledPattern::any(),
            decision: Decision::Allow,
            description: "test allow".into(),
        }
    }

    fn policy() -> Arc<PolicyEngine> {
        let policy = Arc::new(PolicyEngine::new());
        policy.set_namespace_policies("apps", vec![allow_rule()]);
        policy
    }

    struct Harness {
        tunnel: Tunnel,
        server: tokio::task::JoinHandle<Result<(), InterlinkError>>,
        pool: Arc<MuxPool>,
    }

    fn start(
        upstream: SocketAddr,
        policy: Arc<PolicyEngine>,
        per_tunnel: usize,
        global: Arc<Semaphore>,
        max_age: Duration,
    ) -> Harness {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let peer = identity("client");
        let local = identity("server");
        let server = tokio::spawn(serve_connection(
            server,
            upstream,
            peer.clone(),
            local.clone(),
            policy,
            TunnelLimits {
                max_age,
                max_streams_per_tunnel: per_tunnel,
                total_streams: global,
            },
        ));
        let pool = MuxPool::new(per_tunnel);
        let first = pool
            .register_connection(upstream, client, Arc::new(local))
            .unwrap();
        let tunnel = first.tunnel.clone();
        drop(first);
        Harness {
            tunnel,
            server,
            pool,
        }
    }

    async fn admitted(h: &Harness, listener: &TcpListener) -> (MuxStream, TcpStream) {
        let mut stream = h.tunnel.try_reserve().unwrap().open_stream().await.unwrap();
        stream.write_all(b"open").await.unwrap();
        confirm_admitted(stream, listener).await
    }

    async fn confirm_admitted<S>(mut stream: S, listener: &TcpListener) -> (S, TcpStream)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let (mut backend, _) = listener.accept().await.unwrap();
        let mut buf = [0; 4];
        backend.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"open");
        backend.write_all(b"ok").await.unwrap();
        let mut ack = [0; 2];
        stream.read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack, b"ok");
        (stream, backend)
    }

    async fn raw_open(h: &Harness, payload: &[u8]) -> Compat<yamux::Stream> {
        // Deliberately bypass the client budget to model a hostile peer and
        // drive the server's independent reset/policy/drain defenses.
        let (request, response) = oneshot::channel();
        h.tunnel.open_tx.send(request).await.unwrap();
        let mut stream = response.await.unwrap().unwrap().compat();
        stream.write_all(payload).await.unwrap();
        stream
    }

    async fn rejected(h: &Harness) {
        let mut stream = raw_open(h, b"reject").await;
        assert_eq!(
            stream.read(&mut [0; 1]).await.unwrap(),
            0,
            "RST must end only this stream"
        );
    }

    async fn finish<S>(mut stream: S, mut backend: TcpStream)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        stream.shutdown().await.unwrap();
        backend.shutdown().await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        backend.read_to_end(&mut bytes).await.unwrap();
    }

    async fn permits_returned(global: &Semaphore, count: usize) {
        while global.available_permits() != count {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn per_tunnel_cap_resets_excess_and_keeps_tunnel_alive() {
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let global = Arc::new(Semaphore::new(4));
            let h = start(
                listener.local_addr().unwrap(),
                policy(),
                1,
                global.clone(),
                Duration::from_secs(60),
            );
            let (mut first, mut backend) = admitted(&h, &listener).await;
            for _ in 0..4 {
                rejected(&h).await;
            }
            assert_eq!(global.available_permits(), 3);
            let addr = listener.local_addr().unwrap();
            assert!(
                h.pool.pick_under_cap(&addr).is_none(),
                "client pool uses the configured cap too"
            );
            assert!(
                h.pool.pick_any(&addr).is_none(),
                "fallback cannot bypass the cap"
            );
            backend.write_all(b"alive").await.unwrap();
            let mut data = [0; 5];
            first.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"alive");
            finish(first, backend).await;
            permits_returned(&global, 4).await;
            assert!(h.pool.pick_under_cap(&addr).is_some());
            let (next, backend) = admitted(&h, &listener).await;
            finish(next, backend).await;
            h.server.abort();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn global_cap_is_shared_between_tunnels() {
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let global = Arc::new(Semaphore::new(1));
            let h1 = start(
                listener.local_addr().unwrap(),
                policy(),
                2,
                global.clone(),
                Duration::from_secs(60),
            );
            let h2 = start(
                listener.local_addr().unwrap(),
                policy(),
                2,
                global.clone(),
                Duration::from_secs(60),
            );
            let (first, backend) = admitted(&h1, &listener).await;
            rejected(&h2).await;
            assert_eq!(global.available_permits(), 0);
            finish(first, backend).await;
            permits_returned(&global, 1).await;
            let (second, backend) = admitted(&h2, &listener).await;
            finish(second, backend).await;
            h1.server.abort();
            h2.server.abort();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn policy_reload_denies_new_streams_existing_stream_drains() {
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let policy = policy();
            let h = start(
                listener.local_addr().unwrap(),
                policy.clone(),
                4,
                Arc::new(Semaphore::new(4)),
                Duration::from_secs(60),
            );
            let (mut first, mut backend) = admitted(&h, &listener).await;
            let mut deny = allow_rule();
            deny.decision = Decision::Deny("revoked");
            policy.set_namespace_policies("apps", vec![deny]);
            rejected(&h).await;
            backend.write_all(b"drain").await.unwrap();
            let mut data = [0; 5];
            first.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"drain");
            finish(first, backend).await;
            policy.set_namespace_policies("apps", vec![allow_rule()]);
            let (next, backend) = admitted(&h, &listener).await;
            finish(next, backend).await;
            h.server.abort();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn max_age_rejects_new_streams_and_drains_before_closing() {
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let h = start(
                listener.local_addr().unwrap(),
                policy(),
                1,
                Arc::new(Semaphore::new(4)),
                Duration::from_millis(100),
            );
            let (mut first, mut backend) = admitted(&h, &listener).await;
            let mut pending = raw_open(&h, b"open").await;
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert!(!h.server.is_finished(), "active relay must keep draining");
            assert_eq!(
                pending.read(&mut [0; 1]).await.unwrap(),
                0,
                "age expiry cancels queued admissions"
            );
            rejected(&h).await;
            backend.write_all(b"drain").await.unwrap();
            let mut data = [0; 5];
            first.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"drain");
            finish(first, backend).await;
            h.server.await.unwrap().unwrap();
            while h.pool.entry_counts().0 != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pool_churn_evicts_failed_tunnels_and_creation_locks() {
        timeout(TEST_TIMEOUT, async {
            let pool = MuxPool::new(64);
            for port in 1..=100 {
                let addr = SocketAddr::from(([127, 0, 0, 1], port));
                let lock = pool.creation_lock(addr);
                let guard = lock.lock().await;
                let (io, peer) = tokio::io::duplex(1024);
                let _first = pool
                    .register_connection(addr, io, Arc::new(identity("server")))
                    .unwrap();
                drop(guard);
                drop(lock);
                drop(peer);
            }
            while pool.entry_counts() != (0, 0) {
                tokio::task::yield_now().await;
            }
            // Failed/cancelled establishment never needs a registered tunnel.
            for port in 101..=200 {
                drop(pool.creation_lock(SocketAddr::from(([127, 0, 0, 1], port))));
            }
            assert_eq!(pool.entry_counts(), (0, 0));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn creation_lock_remains_single_flight_with_waiters() {
        timeout(TEST_TIMEOUT, async {
            let pool = MuxPool::new(64);
            let addr = SocketAddr::from(([127, 0, 0, 1], 1));
            let first = pool.creation_lock(addr);
            let first_guard = first.lock().await;
            let second = pool.creation_lock(addr);
            assert!(second.try_lock().is_err());
            drop(first_guard);
            drop(first);
            let second_guard = second.lock().await;
            let third = pool.creation_lock(addr);
            assert!(third.try_lock().is_err());
            drop(second_guard);
            drop(second);
            drop(third);
            assert_eq!(pool.entry_counts(), (0, 0));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn idle_driver_exits_without_dropping_active_streams() {
        timeout(TEST_TIMEOUT, async {
            let (io, peer) = tokio::io::duplex(1024);
            let (_tx, rx) = mpsc::channel(1);
            let live = Arc::new(std::sync::atomic::AtomicUsize::new(1));
            let driver_live = live.clone();
            let driver = tokio::spawn(drive_client(
                Connection::new(io.compat(), yamux_config(), Mode::Client),
                rx,
                driver_live,
                Duration::from_millis(20),
            ));
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!driver.is_finished());
            live.store(0, Ordering::Release);
            driver.await.unwrap();
            drop(peer);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn configured_receive_window_bounds_buffer_and_allows_progress() {
        timeout(TEST_TIMEOUT, async {
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (accepted_tx, accepted_rx) = oneshot::channel();
            let server_driver = tokio::spawn(async move {
                let mut conn = Connection::new(server.compat(), yamux_config(), Mode::Server);
                let stream = futures_util::future::poll_fn(|cx| conn.poll_next_inbound(cx))
                    .await
                    .unwrap()
                    .unwrap();
                accepted_tx.send(stream.compat()).unwrap();
                let _next = futures_util::future::poll_fn(|cx| conn.poll_next_inbound(cx)).await;
            });
            let pool = MuxPool::new(64);
            let tunnel = pool
                .register_connection(
                    SocketAddr::from(([127, 0, 0, 1], 1)),
                    client,
                    Arc::new(identity("server")),
                )
                .unwrap();
            let mut stream = tunnel.open_stream().await.unwrap();
            let window = mux::STREAM_WINDOW as usize;
            stream.write_all(&vec![7; window]).await.unwrap();
            let mut inbound = accepted_rx.await.unwrap();
            assert!(
                timeout(Duration::from_millis(30), stream.write_all(&[8]))
                    .await
                    .is_err(),
                "unread stream must exhaust its 256 KiB credit"
            );
            let (trained_tx, trained_rx) = oneshot::channel();
            let (resume_tx, resume_rx) = oneshot::channel();
            let reader = tokio::spawn(async move {
                let mut data = vec![0; window + 1];
                inbound.read_exact(&mut data).await.unwrap();
                assert!(data[..window].iter().all(|byte| *byte == 7));
                assert_eq!(data[window], 8);
                // Exercise RTT/bandwidth auto-tuning before checking the cap
                // again; merely checking the initial default credit is weaker.
                let mut training = vec![0; window * 8];
                inbound.read_exact(&mut training).await.unwrap();
                trained_tx.send(()).unwrap();
                let filled: usize = resume_rx.await.unwrap();
                let mut tail = vec![0; filled + 1];
                inbound.read_exact(&mut tail).await.unwrap();
                assert!(tail[..filled].iter().all(|byte| *byte == 7));
                assert_eq!(tail[filled], 8);
                inbound.shutdown().await.unwrap();
            });
            stream.write_all(&[8]).await.unwrap();
            stream.write_all(&vec![7; window * 8]).await.unwrap();
            trained_rx.await.unwrap();
            // The final training read may consume credit without polling
            // again to replenish it. Fill whatever credit remains, rather
            // than assuming a full window update has already been sent.
            let payload = vec![7; window * 4];
            let mut filled = 0;
            loop {
                match timeout(Duration::from_millis(30), stream.write(&payload[filled..])).await {
                    Ok(Ok(n)) if n > 0 => {
                        filled += n;
                        assert!(
                            filled <= window,
                            "receive window grew beyond configured cap"
                        );
                    }
                    Err(_) => break,
                    other => panic!("unexpected stream write result: {:?}", other),
                }
            }
            resume_tx.send(filled).unwrap();
            stream.write_all(&[8]).await.unwrap();
            stream.shutdown().await.unwrap();
            reader.await.unwrap();
            server_driver.abort();
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn invalid_stream_limits_return_error_without_panicking() {
        timeout(TEST_TIMEOUT, async {
            for limit in [0, usize::MAX] {
                let (_client, server) = tokio::io::duplex(1024);
                let result = serve_connection(
                    server,
                    SocketAddr::from(([127, 0, 0, 1], 1)),
                    identity("client"),
                    identity("server"),
                    policy(),
                    TunnelLimits {
                        max_age: Duration::from_secs(60),
                        max_streams_per_tunnel: limit,
                        total_streams: Arc::new(Semaphore::new(4)),
                    },
                )
                .await;
                assert!(matches!(result, Err(InterlinkError::Config(_))));
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn client_reservations_are_atomic_before_open_and_released_on_drop() {
        timeout(TEST_TIMEOUT, async {
            for cap in [1, 8] {
                let pool = MuxPool::new(cap);
                let addr = SocketAddr::from(([127, 0, 0, 1], 1));
                let (io, _peer) = tokio::io::duplex(1024);
                let first = pool
                    .register_connection(addr, io, Arc::new(identity("server")))
                    .unwrap();
                let tunnel = first.tunnel.clone();
                assert_eq!(tunnel.slots.available_permits(), cap - 1);
                assert_eq!(tunnel.load(), 1, "registration reserves before publishing");
                drop(first);
                let barrier = Arc::new(tokio::sync::Barrier::new(33));
                let mut contenders = Vec::new();
                for _ in 0..32 {
                    let pool = pool.clone();
                    let barrier = barrier.clone();
                    contenders.push(tokio::spawn(async move {
                        barrier.wait().await;
                        pool.pick_under_cap(&addr)
                    }));
                }
                barrier.wait().await;
                let mut leases = Vec::new();
                for contender in contenders {
                    if let Some(lease) = contender.await.unwrap() {
                        leases.push(lease);
                    }
                }
                assert_eq!(leases.len(), cap, "atomic reservation never oversubscribes");
                assert_eq!(tunnel.load(), cap);
                assert_eq!(tunnel.slots.available_permits(), 0);
                assert!(pool.pick_under_cap(&addr).is_none());
                assert!(pool.pick_any(&addr).is_none());
                assert!(
                    pool.can_grow(&addr),
                    "exhaustion must enter the pool growth path"
                );
                drop(leases);
                assert_eq!(tunnel.slots.available_permits(), cap);
                assert_eq!(tunnel.load(), 0);
                assert!(pool.pick_under_cap(&addr).is_some());
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn client_open_failure_and_cancellation_release_reservations() {
        timeout(TEST_TIMEOUT, async {
            let (open_tx, mut open_rx) = mpsc::channel(1);
            let tunnel = Tunnel {
                id: 1,
                open_tx,
                live: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                slots: Arc::new(Semaphore::new(1)),
                peer_identity: Arc::new(identity("server")),
            };
            let lease = tunnel.try_reserve().unwrap();
            let opening = tokio::spawn(lease.open_stream());
            let request = open_rx.recv().await.unwrap();
            assert_eq!(tunnel.slots.available_permits(), 0);
            opening.abort();
            assert!(matches!(opening.await, Err(error) if error.is_cancelled()));
            assert!(request.is_closed());
            assert_eq!(tunnel.slots.available_permits(), 1);
            assert_eq!(tunnel.load(), 0);
            drop(open_rx);
            let result = tunnel.try_reserve().unwrap().open_stream().await;
            assert!(result.is_err());
            assert_eq!(tunnel.slots.available_permits(), 1);
            assert_eq!(tunnel.load(), 0);
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn close_in_flight_is_admitted_within_grace_while_yamux_keeps_pumping() {
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let global = Arc::new(Semaphore::new(1));
            let h = start(
                listener.local_addr().unwrap(),
                policy(),
                1,
                global.clone(),
                Duration::from_secs(60),
            );
            let (first, backend) = admitted(&h, &listener).await;
            let replacement = raw_open(&h, b"open").await;
            // A later stream's immediate reset proves the first replacement
            // reached the server and is held in its one-slot pending queue.
            let mut excess = raw_open(&h, b"excess").await;
            let eof = timeout(ADMISSION_GRACE / 2, excess.read(&mut [0; 1]))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(eof, 0);
            assert_eq!(global.available_permits(), 0);
            // Send FIN only after admission is provably waiting. Blocking the
            // yamux driver on acquire_owned would prevent this close arriving.
            finish(first, backend).await;
            let (replacement, backend) = confirm_admitted(replacement, &listener).await;
            finish(replacement, backend).await;
            permits_returned(&global, 1).await;
            assert!(!h.server.is_finished());
            h.server.abort();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn global_admission_waits_for_another_tunnels_in_flight_close() {
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let global = Arc::new(Semaphore::new(1));
            let h1 = start(
                listener.local_addr().unwrap(),
                policy(),
                1,
                global.clone(),
                Duration::from_secs(60),
            );
            let h2 = start(
                listener.local_addr().unwrap(),
                policy(),
                1,
                global.clone(),
                Duration::from_secs(60),
            );
            let (first, backend) = admitted(&h1, &listener).await;
            let replacement = raw_open(&h2, b"open").await;
            let mut excess = raw_open(&h2, b"excess").await;
            assert_eq!(
                timeout(ADMISSION_GRACE / 2, excess.read(&mut [0; 1]))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            assert_eq!(global.available_permits(), 0);
            finish(first, backend).await;
            let (replacement, backend) = confirm_admitted(replacement, &listener).await;
            finish(replacement, backend).await;
            permits_returned(&global, 1).await;
            h1.server.abort();
            h2.server.abort();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pending_cap_excess_resets_immediately_and_saturation_still_times_out() {
        timeout(TEST_TIMEOUT, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let global = Arc::new(Semaphore::new(1));
            let h = start(
                listener.local_addr().unwrap(),
                policy(),
                1,
                global.clone(),
                Duration::from_secs(60),
            );
            let (mut first, mut backend) = admitted(&h, &listener).await;
            let started = tokio::time::Instant::now();
            let mut pending = raw_open(&h, b"pending").await;
            let mut excess = raw_open(&h, b"excess").await;
            assert_eq!(
                timeout(ADMISSION_GRACE / 2, excess.read(&mut [0; 1]))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            assert!(
                timeout(Duration::from_millis(20), pending.read(&mut [0; 1]))
                    .await
                    .is_err(),
                "first pending admission must not reset early"
            );
            assert_eq!(pending.read(&mut [0; 1]).await.unwrap(), 0);
            assert!(started.elapsed() >= ADMISSION_GRACE);
            assert_eq!(
                global.available_permits(),
                0,
                "timed out waiters must not steal or release the active permit"
            );
            backend.write_all(b"alive").await.unwrap();
            let mut data = [0; 5];
            first.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"alive");
            finish(first, backend).await;
            permits_returned(&global, 1).await;
            let (next, backend) = admitted(&h, &listener).await;
            finish(next, backend).await;
            h.server.abort();
        })
        .await
        .unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pool_growth_serves_300_concurrent_reservations_across_five_tunnels() {
        timeout(TEST_TIMEOUT, async {
            let pool = MuxPool::new(64);
            let addr = SocketAddr::from(([127, 0, 0, 1], 1));
            let peers = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let barrier = Arc::new(tokio::sync::Barrier::new(300));
            let mut tasks = Vec::new();
            for _ in 0..300 {
                let pool = pool.clone();
                let peers = peers.clone();
                let barrier = barrier.clone();
                tasks.push(tokio::spawn(async move {
                    barrier.wait().await;
                    // Match route's existing fast-path / creation-lock /
                    // recheck / grow contract without external TCP backlogs.
                    for _ in 0..4 {
                        if let Some(lease) = pool.pick_under_cap(&addr) {
                            return lease;
                        }
                        let lock = pool.creation_lock(addr);
                        let _creation_guard = lock.lock().await;
                        if let Some(lease) = pool.pick_under_cap(&addr) {
                            return lease;
                        }
                        if pool.can_grow(&addr) {
                            let (io, peer) = tokio::io::duplex(1024);
                            {
                                let mut peer_sockets = peers.lock();
                                peer_sockets.push(peer);
                            }
                            return pool
                                .register_connection(addr, io, Arc::new(identity("server")))
                                .unwrap();
                        }
                    }
                    panic!("a 300-stream burst must not exhaust the pool's 1024-slot capacity");
                }));
            }
            let mut leases = Vec::new();
            for task in tasks {
                leases.push(task.await.unwrap());
            }
            assert_eq!(leases.len(), 300);
            let tunnels = pool.peers.get(&addr).unwrap();
            assert_eq!(tunnels.len(), 5, "growth is not limited to four tunnels");
            assert_eq!(tunnels.iter().map(Tunnel::load).sum::<usize>(), 300);
            assert!(tunnels.iter().all(|tunnel| tunnel.load() <= 64));
            assert!(pool.can_grow(&addr));
        })
        .await
        .unwrap();
    }
}
