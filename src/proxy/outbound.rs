use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::net::{TcpListener, TcpStream};
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
use crate::proxy::pool::ConnectionPool;

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
    config: ProxyConfig,
    connection_semaphore: Arc<Semaphore>,
    listen_port: u16,
    tls_client: Arc<dyn TlsHandshake>,
    policy: Arc<PolicyEngine>,
    discovery: Option<Arc<ServiceDiscovery>>,
    shutdown: Option<watch::Receiver<bool>>,
    active_connections: Arc<AtomicUsize>,
    local_id: SpiffeId,
    connection_pool: Arc<ConnectionPool>,
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
        let local_id = config
            .identity
            .as_ref()
            .and_then(|s| SpiffeId::from_uri(s).ok())
            .unwrap_or_else(|| SpiffeId::new(&config.trust_domain, "default", "proxy"));
        Self {
            config,
            connection_semaphore: Arc::new(Semaphore::new(max_conn)),
            listen_port: port,
            tls_client,
            policy,
            discovery,
            shutdown: None,
            active_connections: Arc::new(AtomicUsize::new(0)),
            local_id,
            connection_pool: ConnectionPool::new(),
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
    async fn resolve_upstream(&self, upstream: &str) -> Result<String, InterlinkError> {
        if upstream.parse::<std::net::SocketAddr>().is_ok() {
            return Ok(upstream.to_string());
        }

        let Some(discovery) = self.discovery.as_ref() else {
            return Ok(upstream.to_string());
        };

        let resolved = discovery.resolve(upstream).await?;
        let Some(first) = resolved.addrs.into_iter().next() else {
            return Err(InterlinkError::DnsResolution(format!(
                "no endpoints for {}",
                upstream
            )));
        };

        let port = upstream
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .unwrap_or(first.port());

        Ok(format!("{}:{}", first.ip(), port))
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    pub async fn run(self: Arc<Self>) {
        let addr = format!("0.0.0.0:{}", self.listen_port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                error!("failed to bind outbound {}: {}", addr, e);
                return;
            }
        };
        info!("interlink outbound proxy listening on {}", addr);

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (stream, peer_addr) = match accept_result {
                        Ok(s) => s,
                        Err(e) => {
                            error!("outbound accept error: {}", e);
                            continue;
                        }
                    };
                    self.handle_accept(stream, peer_addr).await;
                }
                _ = Self::wait_shutdown(self.shutdown.as_ref()) => {
                    info!("outbound proxy received shutdown signal");
                    break;
                }
            }
        }

        self.wait_for_graceful_shutdown().await;
    }

    async fn wait_shutdown(shutdown: Option<&watch::Receiver<bool>>) {
        match shutdown {
            Some(rx) => {
                let mut rx = rx.clone();
                let _ = rx.changed().await;
            }
            None => std::future::pending().await,
        }
    }

    async fn handle_accept(self: &Arc<Self>, stream: TcpStream, peer_addr: std::net::SocketAddr) {
        if let Err(e) = configure_socket(&stream) {
            warn!("failed to configure accepted socket {}: {}", peer_addr, e);
        }

        // Recover original destination from iptables REDIRECT/DNAT.
        let upstream = get_original_dst(&stream).unwrap_or_else(|| {
            warn!(
                "no upstream for outbound connection from {}, dropping",
                peer_addr
            );
            String::new()
        });

        if upstream.is_empty() {
            let _ = stream.into_std().map(|s| {
                let _ = s.shutdown(std::net::Shutdown::Both);
            });
            return;
        }

        let permit = self.connection_semaphore.clone().acquire_owned().await;
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
        upstream: String,
        peer_addr: std::net::SocketAddr,
    ) {
        let start = Instant::now();
        metrics::record_connection_start();

        // Resolve hostname upstreams via DNS when discovery is configured.
        let upstream = match self.resolve_upstream(&upstream).await {
            Ok(addr) => addr,
            Err(e) => {
                warn!("failed to resolve outbound upstream {}: {}", upstream, e);
                metrics::record_connection(0, 0);
                return;
            }
        };

        debug!(
            "handling outbound connection from {} → {}",
            peer_addr, upstream
        );

        // Step 1: Try pooled connection first.
        let mut tls_stream = match self.connection_pool.checkout(&upstream).await {
            Some(pooled) => {
                debug!("outbound pooled connection to {}", upstream);
                // Build an InterlinkTlsStream from the pooled raw TLS stream.
                // We don't have the peer identity cached for pooled connections,
                // so we extract it from the existing TLS session.
                let peer_id = crate::proxy::handshake::extract_identity_from_tls_stream(&pooled)
                    .unwrap_or_else(|_| SpiffeId::new("unknown", "unknown", "unknown"));
                crate::proxy::handshake::TlsStream {
                    inner: pooled,
                    peer_identity: peer_id,
                }
            }
            None => {
                // Establish fresh mTLS to upstream.
                match self.tls_client.connect(&upstream).await {
                    Ok(s) => {
                        metrics::record_handshake(true);
                        s
                    }
                    Err(e) => {
                        warn!("outbound mTLS handshake failed to {}: {}", upstream, e);
                        metrics::record_handshake_error();
                        metrics::record_connection(0, 0);
                        return;
                    }
                }
            }
        };

        let upstream_id = &tls_stream.peer_identity;
        debug!(
            "outbound mTLS connection to {} identity={}",
            upstream, upstream_id
        );

        // Step 2: Policy evaluation (default-deny).
        let decision = self.policy.evaluate(&self.local_id, upstream_id);
        metrics::record_policy(&decision);
        match decision {
            Decision::Allow => {}
            Decision::Deny(reason) => {
                warn!(
                    "policy denied {} → {}: {}",
                    self.local_id, upstream_id, reason
                );
                metrics::record_connection(0, 0);
                return;
            }
        }

        // Step 3: Bidirectional copy.
        let el = start.elapsed();
        debug!(
            "outbound connection: {} → {} handshake={:?}",
            self.local_id, upstream, el
        );

        let copy_result =
            tokio::io::copy_bidirectional(&mut local_stream, &mut tls_stream.inner).await;
        let (bytes_up, bytes_down, pool_ok) = match copy_result {
            Ok((up, down)) => (up, down, true),
            Err(e) => {
                warn!(
                    "bidirectional copy error for {} → {}: {}",
                    self.local_id, upstream, e
                );
                (0, 0, false)
            }
        };

        // Check the (clean) TLS stream back into the pool for reuse.
        if pool_ok && bytes_up + bytes_down > 0 {
            self.connection_pool
                .checkin(&upstream, tls_stream.inner)
                .await;
        }

        metrics::record_connection(bytes_up, bytes_down);
        debug!("done outbound {} → {}", self.local_id, upstream);
    }
}
