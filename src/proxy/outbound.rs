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
use crate::proxy::config::{EgressPolicy, ProxyConfig};
use crate::proxy::configure_socket;
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
    outbound_bind: std::net::IpAddr,
    egress: EgressPolicy,
    original_destination: Arc<dyn crate::proxy::OriginalDestination>,
}

impl OutboundProxy {
    pub fn new(
        config: ProxyConfig,
        tls_client: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
    ) -> Result<Self, InterlinkError> {
        Self::new_with_discovery(config, ports::OUTBOUND_PROXY, tls_client, policy, None)
    }

    /// Constructor with explicit port (used in tests).
    pub fn new_with_port(
        config: ProxyConfig,
        port: u16,
        tls_client: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
    ) -> Result<Self, InterlinkError> {
        Self::new_with_discovery(config, port, tls_client, policy, None)
    }

    /// Constructor with service discovery.
    pub fn new_with_discovery(
        config: ProxyConfig,
        port: u16,
        tls_client: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
        discovery: Option<Arc<ServiceDiscovery>>,
    ) -> Result<Self, InterlinkError> {
        let max_conn = config.max_connections.unwrap_or(1024);
        let local_id = config.local_identity()?;
        let egress = EgressPolicy::parse(&config.egress_subnets)?;
        if config.tunnel_max_age_secs == 0 || config.mux_max_streams_per_tunnel == 0 {
            return Err(InterlinkError::Config(
                "tunnel limits must be positive".into(),
            ));
        }
        let mux_enabled = config.mux;
        let default_upstream = UpstreamTarget::from_config(config.default_upstream);
        Ok(Self {
            connection_semaphore: Arc::new(Semaphore::new(max_conn)),
            default_upstream,
            mux_enabled,
            mux_pool: crate::proxy::mux::MuxPool::new(
                config.mux_max_streams_per_tunnel,
                max_conn,
                // One busy tunnel per runtime worker (see MuxPool::spread).
                tokio::runtime::Handle::try_current()
                    .map_or(1, |runtime| runtime.metrics().num_workers()),
            ),
            listen_port: port,
            tls_client,
            policy,
            discovery,
            shutdown: None,
            active_connections: Arc::new(AtomicUsize::new(0)),
            local_id,
            outbound_bind: config.outbound_bind,
            egress,
            original_destination: Arc::new(crate::proxy::NetfilterDestination),
        })
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
        let name = match upstream {
            UpstreamTarget::Socket(addr) => return Ok(*addr),
            UpstreamTarget::Host(name) => name,
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
        Ok(first)
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    fn listen_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(self.outbound_bind, self.listen_port)
    }

    pub async fn run(self: Arc<Self>) {
        let addr = self.listen_addr();

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
        let upstream = self
            .original_destination
            .get(&stream)
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
                    warn!(
                        local = %self.local_id.sanitized(),
                        peer = %peer_addr,
                        upstream = %upstream,
                        error = %e,
                        "mux stream relay error"
                    );
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
        if !self.egress.allows(upstream.ip()) {
            warn!("egress denied {} → {}", self.local_id.sanitized(), upstream);
            metrics::record_policy(&Decision::Deny(
                "egress destination outside allowed subnets",
            ));
            return Route::Denied;
        }
        if !self.mux_enabled {
            return self.establish(upstream).await;
        }

        // Up to a few attempts: dead tunnels are evicted and retried.
        let mut attempts_used = 0;
        for attempt in 0..4 {
            attempts_used = attempt + 1;
            // Fast path: an existing tunnel with spare capacity, no lock —
            // unless a busy peer should first grow toward one tunnel per worker.
            if !self.mux_pool.wants_spread(&upstream) {
                if let Some(tunnel) = self.mux_pool.pick_under_cap(&upstream) {
                    match self.try_tunnel_stream(tunnel, upstream).await {
                        Some(route) => return route,
                        None => continue, // dead → evicted; retry
                    }
                }
            }

            // Grow the pool under the per-peer creation lock (single-flight —
            // concurrent first connections to a new peer share one handshake
            // instead of each opening a tunnel).
            let lock = self.mux_pool.creation_lock(upstream);
            let guard = lock.lock().await;

            // A concurrent task may have grown it while we waited.
            let spread = self.mux_pool.wants_spread(&upstream);
            if !spread {
                if let Some(tunnel) = self.mux_pool.pick_under_cap(&upstream) {
                    drop(guard);
                    match self.try_tunnel_stream(tunnel, upstream).await {
                        Some(route) => return route,
                        None => continue,
                    }
                }
            }

            if self.mux_pool.can_grow(&upstream) {
                let route = self.establish(upstream).await;
                // Spreading only adds parallelism: the busy tunnels still have
                // capacity when the additional tunnel cannot be established.
                // Pause before releasing the lock so queued openers share too.
                let spread_failed = spread && matches!(route, Route::Failed);
                if spread_failed {
                    self.mux_pool.pause_spread();
                }
                drop(guard);
                if spread_failed {
                    if let Some(tunnel) = self.mux_pool.pick_under_cap(&upstream) {
                        if let Some(route) = self.try_tunnel_stream(tunnel, upstream).await {
                            return route;
                        }
                    }
                }
                return route;
            }
            drop(guard);

            // The pool already spans the admission limit and every tunnel is
            // saturated: pick_any only succeeds if a slot freed up meanwhile;
            // otherwise this attempt fails — bounded by design (H4).
            if let Some(tunnel) = self.mux_pool.pick_any(&upstream) {
                match self.try_tunnel_stream(tunnel, upstream).await {
                    Some(route) => return route,
                    None => continue,
                }
            }
        }
        warn!(
            upstream = %upstream,
            attempts = attempts_used,
            can_grow = self.mux_pool.can_grow(&upstream),
            local = %self.local_id.sanitized(),
            "outbound route exhausted retries"
        );
        Route::Failed
    }

    /// Try to authorize and open a stream on an existing tunnel.
    /// `None` means the tunnel was dead (now evicted) — caller re-establishes.
    async fn try_tunnel_stream(
        &self,
        tunnel: crate::proxy::mux::ReservedTunnel,
        upstream: std::net::SocketAddr,
    ) -> Option<Route> {
        let candidate = (*tunnel).clone();
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
                warn!(
                    upstream = %upstream,
                    peer = %candidate.peer_identity.sanitized(),
                    error = %e,
                    "mux tunnel unusable, evicting"
                );
                self.mux_pool.evict(&upstream, &candidate);
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
            let tunnel = match self
                .mux_pool
                .register(upstream, tls_stream.inner, upstream_id)
            {
                Ok(reserved) => reserved,
                Err(e) => {
                    warn!("failed to register mux tunnel to {}: {}", upstream, e);
                    return Route::Failed;
                }
            };
            let candidate = (*tunnel).clone();
            return match tunnel.open_stream().await {
                Ok(s) => Route::MuxStream(s),
                Err(e) => {
                    warn!("freshly registered tunnel to {} unusable: {}", upstream, e);
                    self.mux_pool.evict(&upstream, &candidate);
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

#[cfg(test)]
mod security_tests {
    use super::*;
    use crate::proxy::test_support::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn outbound_bind_uses_configured_loopback() {
        let (handshake, _) = handshake(false);
        let config = config();
        assert_eq!(
            config.outbound_bind,
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        let proxy =
            OutboundProxy::new_with_port(config, 0, handshake, Arc::new(PolicyEngine::new()))
                .unwrap();
        let listener = crate::proxy::bind_reuseport(proxy.listen_addr()).unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }

    #[tokio::test]
    async fn outbound_direct_listener_drops_without_permit_or_handshake() {
        tokio::time::timeout(WAIT, async {
            let (handshake, _) = handshake(false);
            let (mut client, server, peer) = pair().await;
            let port = server.local_addr().unwrap().port();
            let mut proxy = OutboundProxy::new_with_port(
                config(),
                port,
                handshake.clone(),
                Arc::new(PolicyEngine::new()),
            )
            .unwrap();
            proxy.original_destination = Arc::new(DirectDestination);
            let proxy = Arc::new(proxy);
            proxy.handle_accept(server, peer).await;
            assert_eq!(client.read(&mut [0; 1]).await.unwrap(), 0);
            assert_eq!(proxy.connection_semaphore.available_permits(), 4);
            assert_eq!(proxy.active_connections.load(Ordering::SeqCst), 0);
            assert_eq!(handshake.calls.load(Ordering::SeqCst), 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn outbound_egress_gate_checks_both_families_before_handshake() {
        tokio::time::timeout(WAIT, async {
            for mux in [false, true] {
                for (subnets, destination, allowed) in [
                    (vec!["192.0.2.0/24"], "192.0.2.17:443", true),
                    (vec!["192.0.2.0/24"], "192.0.3.17:443", false),
                    (vec!["2001:db8::/32"], "[2001:db8::17]:443", true),
                    (vec!["2001:db8::/32"], "[2001:db9::17]:443", false),
                    (vec!["192.0.2.0/24"], "[::1]:443", false),
                    (Vec::new(), "127.0.0.1:443", true),
                ] {
                    let (handshake, _) = handshake(false);
                    let mut config = config();
                    config.mux = mux;
                    config.egress_subnets = subnets.into_iter().map(str::to_owned).collect();
                    let proxy = OutboundProxy::new(
                        config,
                        handshake.clone(),
                        Arc::new(PolicyEngine::new()),
                    )
                    .unwrap();
                    let task =
                        tokio::spawn(
                            async move { proxy.route(destination.parse().unwrap()).await },
                        );
                    if allowed {
                        wait_for(|| handshake.calls.load(Ordering::SeqCst) == 1).await;
                        assert!(
                            !task.is_finished(),
                            "fake handshake gate must hold the route"
                        );
                        handshake.release.add_permits(1);
                        assert!(matches!(task.await.unwrap(), Route::Failed));
                    } else {
                        assert!(matches!(task.await.unwrap(), Route::Denied));
                        assert_eq!(handshake.calls.load(Ordering::SeqCst), 0);
                    }
                }
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn outbound_constructors_reject_missing_invalid_identity_and_cidr() {
        let (handshake, _) = handshake(false);
        let policy = Arc::new(PolicyEngine::new());
        for identity in [None, Some("not-spiffe".into())] {
            let mut config = config();
            config.identity = identity;
            assert!(OutboundProxy::new(config.clone(), handshake.clone(), policy.clone()).is_err());
            assert!(OutboundProxy::new_with_port(
                config.clone(),
                4140,
                handshake.clone(),
                policy.clone()
            )
            .is_err());
            assert!(OutboundProxy::new_with_discovery(
                config,
                4140,
                handshake.clone(),
                policy.clone(),
                None
            )
            .is_err());
        }
        for cidr in ["", "127.0.0.1", "127.0.0.1/33", "::1/129", "not-a-cidr"] {
            let mut config = config();
            config.egress_subnets = vec![cidr.into()];
            let result = OutboundProxy::new(config, handshake.clone(), policy.clone());
            let error = result.err().expect("malformed CIDR must fail").to_string();
            assert!(error.contains(&format!("{cidr:?}")), "{error}");
        }
        assert!(OutboundProxy::new(config(), handshake, policy).is_ok());
    }

    #[tokio::test]
    async fn failed_spread_falls_back_to_busy_tunnel_and_pauses_growth() {
        tokio::time::timeout(WAIT, async {
            let (handshake, _) = handshake(false);
            // Exactly one establishment may run; it fails.
            handshake.release.add_permits(1);
            let mut policy = PolicyEngine::new();
            policy.set_default_decision(Decision::Allow);
            let mut proxy = OutboundProxy::new(config(), handshake.clone(), Arc::new(policy))
                .unwrap();
            // Two workers' worth of spread; four-stream tunnels.
            proxy.mux_pool = crate::proxy::mux::MuxPool::new(4, 8, 2);
            let upstream: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
            let (io, _peer) = tokio::io::duplex(64 * 1024);
            let _busy = proxy
                .mux_pool
                .register_connection(
                    upstream,
                    io,
                    Arc::new(SpiffeId::from_uri("spiffe://test.local/ns/default/sa/peer").unwrap()),
                )
                .unwrap();
            let _busy_too = proxy.mux_pool.pick_under_cap(&upstream).unwrap();
            assert!(proxy.mux_pool.wants_spread(&upstream));

            let first = proxy.route(upstream).await;
            assert!(
                matches!(first, Route::MuxStream(_)),
                "a failed spread must not fail a stream the busy tunnel can carry"
            );
            assert_eq!(handshake.calls.load(Ordering::SeqCst), 1);
            let second = proxy.route(upstream).await;
            assert!(matches!(second, Route::MuxStream(_)));
            assert_eq!(
                handshake.calls.load(Ordering::SeqCst),
                1,
                "a paused spread does not retry establishment per stream"
            );
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod resolution_tests {
    use super::*;
    use crate::proxy::test_support::*;

    #[tokio::test]
    async fn outbound_hostname_endpoint_keeps_explicit_port_for_discovery() {
        let (handshake, _) = handshake(false);
        let discovery = Arc::new(ServiceDiscovery::with_resolver(
            Arc::new(ExpectedEndpoint),
            std::time::Duration::from_secs(30),
        ));
        let proxy = OutboundProxy::new_with_discovery(
            config(),
            4140,
            handshake,
            Arc::new(PolicyEngine::new()),
            Some(discovery),
        )
        .unwrap();
        let target = UpstreamTarget::from_config(Some("backend.test:8443".into())).unwrap();
        let resolved = tokio::time::timeout(WAIT, proxy.resolve_upstream(&target))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved, "192.0.2.7:8443".parse().unwrap());
    }

    #[test]
    fn outbound_rejects_zero_tunnel_limits() {
        let (handshake, _) = handshake(false);
        for (age, streams) in [(0, 64), (3600, 0)] {
            let mut config = config();
            config.tunnel_max_age_secs = age;
            config.mux_max_streams_per_tunnel = streams;
            assert!(
                OutboundProxy::new(config, handshake.clone(), Arc::new(PolicyEngine::new()))
                    .is_err()
            );
        }
    }
}
