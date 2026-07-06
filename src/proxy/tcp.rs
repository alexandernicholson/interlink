use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tracing::{debug, error, info, warn};

use crate::common::constants::{buffers, ports};
use crate::common::error::InterlinkError;
use crate::common::identity::SpiffeId;
use crate::discovery::ServiceDiscovery;
use crate::metrics;
use crate::policy::{Decision, PolicyEngine};
use crate::protocol::ProtocolDetector;
use crate::proxy::config::ProxyConfig;
use crate::proxy::configure_socket;
use crate::proxy::get_original_dst;
use crate::proxy::handshake::TlsHandshake;

/// The core TCP proxy with mandatory mTLS (RFC 8446).
///
/// Data flow per connection:
///   1. TCP accept
///   2. mTLS handshake with mandatory client cert (RFC 8446 §2)
///   3. Extract peer SPIFFE identity from cert SAN (RFC 5280 §4.2.1.6)
///   4. Policy evaluation (default-deny)
///   5. Protocol detection
///   6. Upstream forwarding
///   7. Bidirectional data copy
pub struct TcpProxy {
    config: ProxyConfig,
    connection_semaphore: Arc<Semaphore>,
    listen_port: u16,
    tls_server: Arc<dyn TlsHandshake>,
    policy: Arc<PolicyEngine>,
    discovery: Option<Arc<ServiceDiscovery>>,
    shutdown: Option<watch::Receiver<bool>>,
    active_connections: Arc<AtomicUsize>,
}

impl TcpProxy {
    pub fn new(
        config: ProxyConfig,
        tls_server: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
    ) -> Self {
        Self::new_with_discovery(config, ports::INBOUND_PROXY, tls_server, policy, None)
    }

    /// Constructor with explicit port (used in tests).
    pub fn new_with_port(
        config: ProxyConfig,
        port: u16,
        tls_server: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
    ) -> Self {
        Self::new_with_discovery(config, port, tls_server, policy, None)
    }

    /// Constructor with service discovery.
    pub fn new_with_discovery(
        config: ProxyConfig,
        port: u16,
        tls_server: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
        discovery: Option<Arc<ServiceDiscovery>>,
    ) -> Self {
        let max_conn = config.max_connections.unwrap_or(1024);
        Self {
            config,
            connection_semaphore: Arc::new(Semaphore::new(max_conn)),
            listen_port: port,
            tls_server,
            policy,
            discovery,
            shutdown: None,
            active_connections: Arc::new(AtomicUsize::new(0)),
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
        // Already a literal socket address (e.g. 10.0.0.1:8080).
        if upstream.parse::<std::net::SocketAddr>().is_ok() {
            return Ok(upstream.to_string());
        }

        // If no discovery resolver is configured, fall back to the raw string.
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

        // Preserve the port from the original host:port if present.
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
                error!("failed to bind {}: {}", addr, e);
                return;
            }
        };
        info!("interlink proxy listening on {}", addr);

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (stream, peer_addr) = match accept_result {
                        Ok(s) => s,
                        Err(e) => {
                            error!("accept error: {}", e);
                            continue;
                        }
                    };
                    self.handle_accept(stream, peer_addr).await;
                }
                _ = Self::wait_shutdown(self.shutdown.as_ref()) => {
                    info!("inbound proxy received shutdown signal");
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

        // Recover original destination from iptables REDIRECT
        let upstream = get_original_dst(&stream)
            .or_else(|| self.config.default_upstream.clone())
            .unwrap_or_else(|| {
                warn!("no upstream for connection from {}, dropping", peer_addr);
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
                self.active_connections.fetch_add(1, Ordering::SeqCst);
                let this = self.clone();
                tokio::spawn(async move {
                    this.handle_connection(stream, upstream, peer_addr).await;
                    this.active_connections.fetch_sub(1, Ordering::SeqCst);
                    drop(p);
                });
            }
            Err(_) => {
                warn!("connection limit reached, rejecting {}", peer_addr);
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
            let active = self.active_connections.load(Ordering::SeqCst);
            if active == 0 {
                info!("inbound proxy shutdown complete");
                return;
            }
            if Instant::now() >= deadline {
                warn!(
                    "inbound proxy shutdown grace period expired with {} active connections",
                    active
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    async fn handle_connection(
        &self,
        stream: TcpStream,
        upstream: String,
        peer_addr: std::net::SocketAddr,
    ) {
        let start = Instant::now();
        metrics::record_connection_start();

        // Resolve hostname upstreams via DNS when discovery is configured.
        let upstream = match self.resolve_upstream(&upstream).await {
            Ok(addr) => addr,
            Err(e) => {
                warn!("failed to resolve upstream {}: {}", upstream, e);
                metrics::record_connection(0, 0);
                return;
            }
        };

        debug!("handling connection from {} → {}", peer_addr, upstream);

        // Step 1: mTLS handshake (RFC 8446 §2 Figure 1)
        // Note: the TcpStream is consumed — we can't get SO_ORIGINAL_DST here.
        let tls_stream = match self.tls_server.accept(stream).await {
            Ok(s) => s,
            Err(e) => {
                warn!("mTLS handshake failed from {}: {}", peer_addr, e);
                metrics::record_handshake_error();
                metrics::record_connection(0, 0);
                return;
            }
        };
        metrics::record_handshake(true);

        let peer_id = &tls_stream.peer_identity;
        info!("mTLS connection from {} identity={}", peer_addr, peer_id);

        // Step 2: Policy evaluation (default-deny)
        let local_id = self
            .config
            .identity
            .as_ref()
            .and_then(|s| SpiffeId::from_uri(s).ok())
            .unwrap_or_else(|| SpiffeId::new(&self.config.trust_domain, "default", "proxy"));

        let decision = self.policy.evaluate(peer_id, &local_id);
        metrics::record_policy(&decision);
        match decision {
            Decision::Allow => {}
            Decision::Deny(reason) => {
                warn!("policy denied {} → {}: {}", peer_id, local_id, reason);
                metrics::record_connection(0, 0);
                return;
            }
        }

        // Step 3: Protocol detection on the decrypted stream.
        // We read a small peek buffer, detect the protocol, then replay those
        // bytes to the upstream before entering the full bidirectional copy.
        let mut detect_buf = vec![0u8; buffers::PROTOCOL_DETECT];
        let mut tls_reader = tls_stream.inner;
        let detect_len = match tls_reader.read(&mut detect_buf).await {
            Ok(0) => {
                debug!("client {} closed before sending data", peer_id);
                metrics::record_connection(0, 0);
                return;
            }
            Ok(n) => n,
            Err(e) => {
                warn!("protocol detection read failed for {}: {}", peer_id, e);
                metrics::record_connection(0, 0);
                return;
            }
        };
        detect_buf.truncate(detect_len);
        let detected = ProtocolDetector::detect(&detect_buf);
        info!(
            "detected protocol for {} → {}: {:?}",
            peer_id, upstream, detected
        );

        // Step 4: Connect to upstream
        let mut upstream_stream = match TcpStream::connect(&upstream).await {
            Ok(s) => s,
            Err(e) => {
                warn!("upstream connect failed to {}: {}", upstream, e);
                metrics::record_connection(0, 0);
                return;
            }
        };
        if let Err(e) = configure_socket(&upstream_stream) {
            warn!("failed to configure upstream socket {}: {}", upstream, e);
        }

        // Step 5: Replay the peeked bytes then start bidirectional copy.
        let el = start.elapsed();
        info!(
            "connection: {} → {} handshake={:?} protocol={:?}",
            peer_id, upstream, el, detected
        );

        if let Err(e) = upstream_stream.write_all(&detect_buf).await {
            warn!(
                "failed to write detected bytes to upstream {}: {}",
                upstream, e
            );
            metrics::record_connection(0, 0);
            return;
        }

        let copy_result =
            tokio::io::copy_bidirectional(&mut tls_reader, &mut upstream_stream).await;
        let (bytes_up, bytes_down) = match copy_result {
            Ok((up, down)) => (up, down),
            Err(e) => {
                warn!(
                    "bidirectional copy error for {} → {}: {}",
                    peer_id, upstream, e
                );
                (0, 0)
            }
        };

        metrics::record_connection(bytes_up, bytes_down);
        debug!("done {} → {}", peer_id, upstream);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_proxy_copy_basic() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(b"hello").await.unwrap();
        drop(a);
        let mut buf = String::new();
        b.read_to_string(&mut buf).await.unwrap();
        assert_eq!(buf, "hello");
    }

    #[tokio::test]
    async fn test_proxy_copy_large() {
        let data = vec![0xABu8; 65536];
        let (mut a, mut b) = tokio::io::duplex(65536);
        a.write_all(&data).await.unwrap();
        drop(a);
        let mut buf = Vec::new();
        b.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf.len(), 65536);
    }
}
