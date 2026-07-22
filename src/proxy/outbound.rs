#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::net::TcpStream;
use tokio::sync::{watch, Semaphore};
use tracing::{debug, error, info, warn};

use crate::common::constants::ports;
use crate::common::error::InterlinkError;
use crate::common::identity::SpiffeId;
use crate::discovery::ServiceDiscovery;
use crate::metrics;
use crate::policy::{Decision, PolicyEngine};
use crate::proxy::config::ProxyConfig;
use crate::proxy::configure_socket;
use crate::proxy::get_original_dst;
use crate::proxy::handshake::TlsHandshake;
use crate::proxy::UpstreamTarget;

/// The outbound TCP proxy that wraps local plaintext connections in mTLS.
///
/// Data flow per connection:
///   1. TCP accept from local application (via iptables REDIRECT/DNAT)
///   2. Recover original destination
///   3. Establish mTLS to upstream as a client
///   4. Extract upstream SPIFFE identity
///   5. Policy evaluation (default-deny)
///   6. Bidirectional data copy
pub struct OutboundProxy {
    connection_semaphore: Arc<Semaphore>,
    default_upstream: Option<UpstreamTarget>,
    mux_enabled: bool,
    mux_pool: Arc<crate::proxy::mux::MuxPool>,
    listen_port: u16,
    tls_client: Arc<dyn TlsHandshake>,
    policy: Arc<PolicyEngine>,
    discovery: Option<Arc<ServiceDiscovery>>,
    shutdown: Option<watch::Receiver<bool>>,
    active_connections: Arc<AtomicUsize>,
    local_id: SpiffeId,
}

impl OutboundProxy {
    pub fn new(
        config: ProxyConfig,
        tls_client: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
    ) -> Self {
        Self::new_with_discovery(config, ports::OUTBOUND_PROXY, tls_client, policy, None)
    }

    /// Constructor with explicit port (used in tests).
    pub fn new_with_port(
        config: ProxyConfig,
        port: u16,
        tls_client: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
    ) -> Self {
        Self::new_with_discovery(config, port, tls_client, policy, None)
    }

    /// Constructor with service discovery.
    pub fn new_with_discovery(
        config: ProxyConfig,
        port: u16,
        tls_client: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
        discovery: Option<Arc<ServiceDiscovery>>,
    ) -> Self {
        let max_conn = config.max_connections.unwrap_or(1024);
        #[cfg_attr(not(test), allow(clippy::expect_used))]
        let local_id = config
            .identity
            .as_ref()
            .and_then(|s| SpiffeId::from_uri(s).ok())
            .unwrap_or_else(|| {
                SpiffeId::try_new(&config.trust_domain, "default", "proxy")
                    .expect("trust_domain validated in Config::validate")
            });
        let mux_enabled = config.mux;
        let default_upstream = UpstreamTarget::from_config(config.default_upstream);
        Self {
            connection_semaphore: Arc::new(Semaphore::new(max_conn)),
            default_upstream,
            mux_enabled,
            mux_pool: crate::proxy::mux::MuxPool::new(),
            listen_port: port,
            tls_client,
            policy,
            discovery,
            shutdown: None,
            active_connections: Arc::new(AtomicUsize::new(0)),
            local_id,
        }
    }

    /// Attach a service discovery resolver.
    pub fn with_discovery(mut self, discovery: Arc<ServiceDiscovery>) -> Self {
        self.discovery = Some(discovery);
        self
    }

    /// Attach a shutdown signal receiver.
    pub fn with_shutdown(mut self, shutdown: watch::Receiver<bool>) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    /// Resolve a hostname upstream to an IP:port via DNS.
    ///
    /// If the upstream is already a socket address, it is returned unchanged.
    async fn resolve_upstream(
        &self,
        upstream: &UpstreamTarget,
    ) -> Result<std::net::SocketAddr, InterlinkError> {
        let (name, port) = match upstream {
            UpstreamTarget::Socket(addr) => return Ok(*addr),
            UpstreamTarget::Host { name, port } => (name, port),
        };
        let Some(discovery) = self.discovery.as_ref() else {
            return Err(InterlinkError::DnsResolution(format!(
                "cannot resolve hostname '{}' without discovery",
                upstream
            )));
        };
        let resolved = discovery.resolve(name).await?;
        let first = resolved.addrs.first().copied().ok_or_else(|| {
            InterlinkError::DnsResolution(format!("no endpoints for {}", upstream))
        })?;
        Ok(std::net::SocketAddr::new(
            first.ip(),
            port.unwrap_or_else(|| first.port()),
        ))
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    pub async fn run(self: Arc<Self>) {
        let addr: std::net::SocketAddr = ([0, 0, 0, 0], self.listen_port).into();

        if self.listen_port == 0 {
            info!("outbound proxy disabled (port 0)");
            return;
        }

        // Bind N SO_REUSEPORT listeners up front so bind/listen failures are
        // handled here (R26): log and skip a failed acceptor, refuse to run
        // with zero. Panicking in a spawned task would abort the process
        // under panic = "abort" (B8).
        let mut listeners = Vec::new();
        for i in 0..crate::proxy::num_acceptors() {
            match crate::proxy::bind_reuseport(addr) {
                Ok(l) => listeners.push(l),
                Err(e) => error!("failed to bind outbound acceptor {} on {}: {}", i, addr, e),
            }
        }
        if listeners.is_empty() {
            error!(
                "no acceptors could bind outbound {}; proxy not started",
                addr
            );
            return;
        }
        info!(
            "interlink outbound proxy listening on {} with {} acceptors",
            addr,
            listeners.len()
        );

        let acceptors: Vec<_> = listeners
            .into_iter()
            .enumerate()
            .map(|(id, listener)| {
                // Each acceptor selects on its own clone of the watch channel
                // (R27): with_shutdown() must stop the accept loops.
                let shutdown = self.shutdown.clone();
                let proxy = self.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            accept_result = listener.accept() => {
                                let (stream, peer_addr) = match accept_result {
                                    Ok(s) => s,
                                    Err(e) => {
                                        error!("outbound acceptor {} accept error: {}", id, e);
                                        continue;
                                    }
                                };
                                proxy.handle_accept(stream, peer_addr).await;
                            }
                            _ = crate::proxy::wait_shutdown(shutdown.clone()) => {
                                info!("outbound acceptor {} received shutdown signal", id);
                                break;
                            }
                        }
                    }
                })
            })
            .collect();

        for h in acceptors {
            let _ = h.await;
        }

        self.wait_for_graceful_shutdown().await;
    }
}

impl OutboundProxy {
    async fn handle_accept(self: &Arc<Self>, stream: TcpStream, peer_addr: std::net::SocketAddr) {
        if let Err(e) = configure_socket(&stream) {
            warn!("failed to configure accepted socket {}: {}", peer_addr, e);
        }

        // Recover original destination from iptables REDIRECT/DNAT. Without a
        // redirect, SO_ORIGINAL_DST returns the connection's *actual*
        // destination — i.e. this proxy's own listen address (conntrack has an
        // entry for every connection, redirected or not). An original dst on
        // our own port therefore means "not redirected": fall back to the
        // configured default upstream instead of connecting to ourselves.
        let upstream = get_original_dst(&stream)
            .filter(|sa| sa.port() != self.listen_port)
            .map(UpstreamTarget::Socket)
            .or_else(|| self.default_upstream.clone());
        let Some(upstream) = upstream else {
            warn!(
                "no upstream for outbound connection from {}, dropping",
                peer_addr
            );
            let _ = stream.into_std().map(|s| {
                let _ = s.shutdown(std::net::Shutdown::Both);
            });
            return;
        };

        let permit = self.connection_semaphore.clone().try_acquire_owned();
        match permit {
            Ok(p) => {
                self.active_connections.fetch_add(1, Ordering::Release);
                let this = self.clone();
                tokio::spawn(async move {
                    this.handle_connection(stream, upstream, peer_addr).await;
                    this.active_connections.fetch_sub(1, Ordering::Release);
                    drop(p);
                });
            }
            Err(_) => {
                warn!("outbound connection limit reached, rejecting {}", peer_addr);
                metrics::record_saturation_rejection();
                let _ = stream.into_std().map(|s| {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                });
            }
        }
    }

    async fn wait_for_graceful_shutdown(&self) {
        let grace = crate::common::constants::timeouts::SHUTDOWN_GRACE;
        let deadline = Instant::now() + grace;

        loop {
            if self.active_connections.load(Ordering::Acquire) == 0 {
                info!("outbound proxy shutdown complete");
                return;
            }
            if Instant::now() >= deadline {
                warn!(
                    "outbound proxy shutdown grace period expired with {} active connections",
                    self.active_connections.load(Ordering::Relaxed)
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    async fn handle_connection(
        &self,
        mut local_stream: TcpStream,
        upstream: UpstreamTarget,
        peer_addr: std::net::SocketAddr,
    ) {
        let start = Instant::now();
        metrics::record_connection_start();

        let upstream = match self.resolve_upstream(&upstream).await {
            Ok(addr) => addr,
            Err(e) => {
                warn!("failed to resolve outbound upstream {}: {}", upstream, e);
                metrics::record_connection_failed();
                return;
            }
        };

        debug!(
            "handling outbound connection from {} → {}",
            peer_addr, upstream
        );

        // Route the connection: over a shared mux tunnel stream when the peer
        // supports it, else a dedicated (legacy) TLS connection. The tunnel
        // creation lock is held only while establishing, never across relays.
        match self.route(upstream).await {
            Route::MuxStream(mut stream) => {
                metrics::record_mux_stream();
                if let Err(e) = crate::proxy::record_relay_result(
                    crate::proxy::copy_bidirectional(&mut local_stream, &mut stream).await,
                    start,
                ) {
                    debug!("mux stream relay error → {}: {}", upstream, e);
                }
            }
            Route::Legacy(mut tls) => {
                if let Err(e) = crate::proxy::record_relay_result(
                    crate::proxy::copy_bidirectional(&mut local_stream, tls.as_mut()).await,
                    start,
                ) {
                    warn!(
                        "bidirectional copy error for {} → {}: {}",
                        self.local_id, upstream, e
                    );
                }
            }
            Route::Denied | Route::Failed => {
                metrics::record_connection_failed();
            }
        }
        debug!("done outbound {} → {}", self.local_id, upstream);
    }

    /// Decide how this connection reaches `upstream`.
    async fn route(&self, upstream: std::net::SocketAddr) -> Route {
        if !self.mux_enabled {
            return self.establish(upstream).await;
        }

        // Up to a few attempts: dead tunnels are evicted and retried.
        for _ in 0..4 {
            // Fast path: an existing tunnel with spare capacity, no lock.
            if let Some(tunnel) = self.mux_pool.pick_under_cap(&upstream) {
                match self.try_tunnel_stream(&tunnel, upstream).await {
                    Some(route) => return route,
                    None => continue, // dead → evicted; retry
                }
            }

            // No tunnel has spare capacity. Grow the pool under the per-peer
            // creation lock (single-flight — concurrent first connections to a
            // new peer share one handshake instead of each opening a tunnel).
            let lock = self.mux_pool.creation_lock(upstream);
            let guard = lock.lock().await;

            // A concurrent task may have grown it while we waited.
            if let Some(tunnel) = self.mux_pool.pick_under_cap(&upstream) {
                drop(guard);
                match self.try_tunnel_stream(&tunnel, upstream).await {
                    Some(route) => return route,
                    None => continue,
                }
            }

            if self.mux_pool.can_grow(&upstream) {
                let route = self.establish(upstream).await;
                drop(guard);
                return route;
            }
            drop(guard);

            // Pool is at MAX_TUNNELS_PER_PEER and all are at the soft cap: use
            // the least-loaded tunnel anyway (still under yamux's hard 512 cap).
            if let Some(tunnel) = self.mux_pool.pick_any(&upstream) {
                match self.try_tunnel_stream(&tunnel, upstream).await {
                    Some(route) => return route,
                    None => continue,
                }
            }
        }
        Route::Failed
    }

    /// Try to authorize and open a stream on an existing tunnel.
    /// `None` means the tunnel was dead (now evicted) — caller re-establishes.
    async fn try_tunnel_stream(
        &self,
        tunnel: &crate::proxy::mux::Tunnel,
        upstream: std::net::SocketAddr,
    ) -> Option<Route> {
        // Every stream is authorized against the tunnel's peer identity,
        // exactly as a dedicated connection would be.
        let decision = self.policy.evaluate(&self.local_id, &tunnel.peer_identity);
        metrics::record_policy(&decision);
        if let Decision::Deny(reason) = decision {
            warn!(
                "policy denied {} → {}: {}",
                self.local_id, tunnel.peer_identity, reason
            );
            return Some(Route::Denied);
        }
        match tunnel.open_stream().await {
            Ok(s) => Some(Route::MuxStream(s)),
            Err(e) => {
                debug!("mux tunnel to {} unusable ({}); evicting", upstream, e);
                self.mux_pool.evict(&upstream, tunnel);
                None
            }
        }
    }

    /// Establish a fresh TLS connection: policy-check the peer, then either
    /// promote it to a shared tunnel (peer negotiated mux) or use it 1:1.
    async fn establish(&self, upstream: std::net::SocketAddr) -> Route {
        let handshake_start = Instant::now();
        let tls_stream = match self.tls_client.connect(upstream).await {
            Ok(s) => {
                metrics::record_handshake(handshake_start.elapsed());
                s
            }
            Err(e) => {
                warn!("outbound mTLS handshake failed to {}: {}", upstream, e);
                metrics::record_handshake_error();
                return Route::Failed;
            }
        };

        let upstream_id = tls_stream.peer_identity.clone();
        debug!(
            "outbound mTLS connection to {} identity={}",
            upstream, upstream_id
        );

        let decision = self.policy.evaluate(&self.local_id, &upstream_id);
        metrics::record_policy(&decision);
        if let Decision::Deny(reason) = decision {
            warn!(
                "policy denied {} → {}: {}",
                self.local_id, upstream_id, reason
            );
            return Route::Denied;
        }

        // ALPN commits the wire protocol: if the handshake negotiated mux we
        // MUST speak mux, whatever the local flag says (the flag controls what
        // the TlsClient offers — wired in main.rs — not what was agreed).
        if crate::proxy::mux::negotiated_alpn(&tls_stream.inner)
            == Some(crate::proxy::mux::ALPN_MUX)
        {
            let tunnel = self
                .mux_pool
                .register(upstream, tls_stream.inner, upstream_id);
            return match tunnel.open_stream().await {
                Ok(s) => Route::MuxStream(s),
                Err(e) => {
                    warn!("freshly registered tunnel to {} unusable: {}", upstream, e);
                    self.mux_pool.evict(&upstream, &tunnel);
                    Route::Failed
                }
            };
        }

        Route::Legacy(Box::new(tls_stream.inner))
    }
}

/// How an outbound connection reaches its upstream.
enum Route {
    /// A stream over a shared mux tunnel (no per-connection handshake).
    MuxStream(crate::proxy::mux::MuxStream),
    /// A dedicated 1:1 TLS connection (legacy peers / mux disabled).
    /// Boxed: a rustls stream is ~1.2 KB vs ~72 B for the other variants.
    Legacy(Box<tokio_rustls::TlsStream<TcpStream>>),
    Denied,
    Failed,
}
