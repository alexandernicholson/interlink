use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tracing::{debug, error, info, warn};

use crate::common::constants::buffers;
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

pub struct TcpProxy {
    config: ProxyConfig,
    connection_semaphore: Arc<Semaphore>,
    listen_port: u16,
    tls_server: Arc<dyn TlsHandshake>,
    policy: Arc<PolicyEngine>,
    discovery: Option<Arc<ServiceDiscovery>>,
    shutdown: Option<watch::Receiver<bool>>,
    active_connections: Arc<AtomicUsize>,
    local_id: SpiffeId,
}

impl TcpProxy {
    pub fn new(
        config: ProxyConfig,
        tls_server: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
    ) -> Self {
        Self::new_with_discovery(
            config,
            crate::common::constants::ports::INBOUND_PROXY,
            tls_server,
            policy,
            None,
        )
    }

    pub fn new_with_port(
        config: ProxyConfig,
        port: u16,
        tls_server: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
    ) -> Self {
        Self::new_with_discovery(config, port, tls_server, policy, None)
    }

    pub fn new_with_discovery(
        config: ProxyConfig,
        port: u16,
        tls_server: Arc<dyn TlsHandshake>,
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
            tls_server,
            policy,
            discovery,
            shutdown: None,
            active_connections: Arc::new(AtomicUsize::new(0)),
            local_id,
        }
    }

    pub fn with_discovery(mut self, discovery: Arc<ServiceDiscovery>) -> Self {
        self.discovery = Some(discovery);
        self
    }

    pub fn with_shutdown(mut self, shutdown: watch::Receiver<bool>) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

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

        let upstream = match &self.config.default_upstream {
            Some(dst) => dst.clone(),
            None => get_original_dst(&stream)
                .map(|sa| sa.to_string())
                .unwrap_or_else(|| {
                    warn!("no upstream for connection from {}, dropping", peer_addr);
                    String::new()
                }),
        };

        if upstream.is_empty() {
            let _ = stream.into_std().map(|s| {
                let _ = s.shutdown(std::net::Shutdown::Both);
            });
            return;
        }

        // Use try_acquire_owned to avoid head-of-line blocking — the accept
        // loop never stalls when the connection limit is reached.
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
            if self.active_connections.load(Ordering::Acquire) == 0 {
                info!("inbound proxy shutdown complete");
                return;
            }
            if Instant::now() >= deadline {
                warn!(
                    "inbound proxy shutdown grace period expired with {} active connections",
                    self.active_connections.load(Ordering::Relaxed)
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

        let upstream = match self.resolve_upstream(&upstream).await {
            Ok(addr) => addr,
            Err(e) => {
                warn!("failed to resolve upstream {}: {}", upstream, e);
                metrics::record_connection(0, 0, std::time::Duration::ZERO);
                return;
            }
        };

        debug!("handling connection from {} → {}", peer_addr, upstream);

        // Overlap the upstream TCP connect with the mTLS handshake.
        let handshake_start = Instant::now();
        let (tls_result, upstream_result) = tokio::join!(
            self.tls_server.accept(stream),
            TcpStream::connect(&upstream),
        );

        let tls_stream = match tls_result {
            Ok(s) => s,
            Err(e) => {
                warn!("mTLS handshake failed from {}: {}", peer_addr, e);
                metrics::record_handshake_error();
                if let Ok(up) = upstream_result {
                    let _ = up.into_std().map(|s| s.shutdown(std::net::Shutdown::Both));
                }
                metrics::record_connection(0, 0, std::time::Duration::ZERO);
                return;
            }
        };
        metrics::record_handshake(handshake_start.elapsed());

        let mut upstream_stream = match upstream_result {
            Ok(s) => s,
            Err(e) => {
                warn!("upstream connect failed to {}: {}", upstream, e);
                metrics::record_connection(0, 0, std::time::Duration::ZERO);
                return;
            }
        };
        if let Err(e) = configure_socket(&upstream_stream) {
            warn!("failed to configure upstream socket {}: {}", upstream, e);
        }

        let peer_id = &tls_stream.peer_identity;
        debug!("mTLS connection from {} identity={}", peer_addr, peer_id);

        let decision = self.policy.evaluate(peer_id, &self.local_id);
        metrics::record_policy(&decision);
        match decision {
            Decision::Allow => {}
            Decision::Deny(reason) => {
                warn!("policy denied {} → {}: {}", peer_id, self.local_id, reason);
                let _ = upstream_stream
                    .into_std()
                    .map(|s| s.shutdown(std::net::Shutdown::Both));
                metrics::record_connection(0, 0, std::time::Duration::ZERO);
                return;
            }
        }

        let mut detect_buf = [0u8; buffers::PROTOCOL_DETECT];
        let mut tls_reader = tls_stream.inner;
        let detect_len = match tls_reader.read(&mut detect_buf).await {
            Ok(0) => {
                debug!("client {} closed before sending data", peer_id);
                metrics::record_connection(0, 0, std::time::Duration::ZERO);
                return;
            }
            Ok(n) => n,
            Err(e) => {
                warn!("protocol detection read failed for {}: {}", peer_id, e);
                metrics::record_connection(0, 0, std::time::Duration::ZERO);
                return;
            }
        };
        let detected = ProtocolDetector::detect(&detect_buf[..detect_len]);
        debug!(
            "detected protocol for {} → {}: {:?}",
            peer_id, upstream, detected
        );

        let el = start.elapsed();
        debug!(
            "connection: {} → {} handshake={:?} protocol={:?}",
            peer_id, upstream, el, detected
        );

        if let Err(e) = upstream_stream.write_all(&detect_buf[..detect_len]).await {
            warn!(
                "failed to write detected bytes to upstream {}: {}",
                upstream, e
            );
            metrics::record_connection(0, 0, std::time::Duration::ZERO);
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

        metrics::record_connection(bytes_up, bytes_down, start.elapsed());
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
