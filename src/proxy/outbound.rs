#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    listen_port: u16,
    tls_client: Arc<dyn TlsHandshake>,
    policy: Arc<PolicyEngine>,
    discovery: Option<Arc<ServiceDiscovery>>,
    shutdown: Option<watch::Receiver<bool>>,
    shutdown_flag: Arc<AtomicBool>,
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
        Self {
            connection_semaphore: Arc::new(Semaphore::new(max_conn)),
            listen_port: port,
            tls_client,
            policy,
            discovery,
            shutdown: None,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
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

    /// Install the shared shutdown flag for multi-acceptor.
    pub fn with_shutdown_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.shutdown_flag = flag;
        self
    }

    /// Resolve a hostname upstream to an IP:port via DNS.
    ///
    /// If the upstream is already a socket address, it is returned unchanged.
    async fn resolve_upstream(
        &self,
        upstream: &str,
    ) -> Result<std::net::SocketAddr, InterlinkError> {
        if let Ok(sa) = upstream.parse::<std::net::SocketAddr>() {
            return Ok(sa);
        }
        let Some(discovery) = self.discovery.as_ref() else {
            return Err(InterlinkError::DnsResolution(format!(
                "cannot resolve hostname '{}' without discovery",
                upstream
            )));
        };
        let resolved = discovery.resolve(upstream).await?;
        let first = resolved.addrs.into_iter().next().ok_or_else(|| {
            InterlinkError::DnsResolution(format!("no endpoints for {}", upstream))
        })?;
        let port = upstream
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .unwrap_or(first.port());
        Ok(std::net::SocketAddr::new(first.ip(), port))
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

        let num_acceptors = std::cmp::min(
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2),
            4,
        ).max(2);
        info!(
            "interlink outbound proxy listening on {} with {} acceptors",
            addr, num_acceptors
        );

        let shutdown_flag = self.shutdown_flag.clone();
        let acceptors: Vec<_> = (0..num_acceptors)
            .map(|i| create_outbound_acceptor(addr, i, self.clone(), shutdown_flag.clone()))
            .collect();

        for h in acceptors {
            let _ = h.await;
        }

        self.wait_for_graceful_shutdown().await;
    }
}

/// Create a SO_REUSEPORT socket for the outbound proxy and spawn an acceptor.
#[cfg_attr(not(test), allow(clippy::expect_used))]
fn create_outbound_acceptor(
    addr: std::net::SocketAddr,
    id: usize,
    proxy: Arc<OutboundProxy>,
    shutdown_flag: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let domain = if addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    tokio::spawn(async move {
        let sock = socket2::Socket::new(
            domain,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .expect("create socket");
        sock.set_reuse_address(true).ok();
        #[cfg(target_os = "linux")]
        sock.set_reuse_port(true).ok();
        sock.set_nonblocking(true).ok();
        sock.bind(&socket2::SockAddr::from(addr))
            .unwrap_or_else(|e| panic!("bind outbound acceptor {}: {}", id, e));
        sock.listen(1024)
            .unwrap_or_else(|e| panic!("listen outbound acceptor {}: {}", id, e));
        let std_listener: std::net::TcpListener = sock.into();
        let listener = TcpListener::from_std(std_listener)
            .unwrap_or_else(|e| panic!("tokio listener outbound acceptor {}: {}", id, e));

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
                _ = async { while !shutdown_flag.load(Ordering::Acquire) {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                } } => {
                    info!("outbound acceptor {} received shutdown signal", id);
                    break;
                }
            }
        }
    })
}

impl OutboundProxy {
    async fn handle_accept(self: &Arc<Self>, stream: TcpStream, peer_addr: std::net::SocketAddr) {
        if let Err(e) = configure_socket(&stream) {
            warn!("failed to configure accepted socket {}: {}", peer_addr, e);
        }

        // Recover original destination from iptables REDIRECT/DNAT.
        let upstream = get_original_dst(&stream)
            .map(|sa| sa.to_string())
            .unwrap_or_else(|| {
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
        upstream: String,
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

        let handshake_start = Instant::now();
        let upstream_str = upstream.to_string();
        let mut tls_stream = match self.tls_client.connect(&upstream_str).await {
            Ok(s) => {
                metrics::record_handshake(handshake_start.elapsed());
                s
            }
            Err(e) => {
                warn!("outbound mTLS handshake failed to {}: {}", upstream, e);
                metrics::record_handshake_error();
                metrics::record_connection_failed();
                return;
            }
        };

        let upstream_id = &tls_stream.peer_identity;
        debug!(
            "outbound mTLS connection to {} identity={}",
            upstream, upstream_id
        );

        let decision = self.policy.evaluate(&self.local_id, upstream_id);
        metrics::record_policy(&decision);
        match decision {
            Decision::Allow => {}
            Decision::Deny(reason) => {
                warn!(
                    "policy denied {} → {}: {}",
                    self.local_id, upstream_id, reason
                );
                metrics::record_connection_failed();
                return;
            }
        }

        let el = start.elapsed();
        debug!(
            "outbound connection: {} → {} handshake={:?}",
            self.local_id, upstream, el
        );

        let copy_result =
            crate::proxy::copy_bidirectional(&mut local_stream, &mut tls_stream.inner).await;
        let (bytes_up, bytes_down) = match copy_result {
            Ok((up, down)) => (up, down),
            Err(e) => {
                warn!(
                    "bidirectional copy error for {} → {}: {}",
                    self.local_id, upstream, e
                );
                (0, 0)
            }
        };

        metrics::record_connection(bytes_up, bytes_down, start.elapsed());
        debug!("done outbound {} → {}", self.local_id, upstream);
    }
}
