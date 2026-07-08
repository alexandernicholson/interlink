#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// Shared shutdown flag for multi-acceptor — cloned before spawning tasks.
    shutdown_flag: Arc<AtomicBool>,
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
        // B8: try_new validates at the identity boundary, even though the
        // config has already been checked at startup. The unwrap is safe
        // because Config::validate() enforces non-empty trust_domain.
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
            config,
            connection_semaphore: Arc::new(Semaphore::new(max_conn)),
            listen_port: port,
            tls_server,
            policy,
            discovery,
            shutdown: None,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
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

    /// Install the shared shutdown flag alongside the watch receiver.
    pub fn with_shutdown_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.shutdown_flag = flag;
        self
    }

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

        // Spawn N acceptor tasks, each with its own SO_REUSEPORT socket,
        // so accept + handshake setup scales across cores at high conn rates.
        let num_acceptors = std::cmp::min(
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2),
            4,
        ).max(2);
        info!(
            "interlink proxy listening on {} with {} acceptors",
            addr, num_acceptors
        );

        let shutdown_flag = self.shutdown_flag.clone();
        let acceptors: Vec<_> = (0..num_acceptors)
            .map(|i| create_acceptor(addr, i, self.clone(), shutdown_flag.clone()))
            .collect();

        for h in acceptors {
            let _ = h.await;
        }

        self.wait_for_graceful_shutdown().await;
    }
}

/// Create a SO_REUSEPORT socket, bind, listen, and spawn an acceptor task.
#[cfg_attr(not(test), allow(clippy::expect_used))]
fn create_acceptor(
    addr: std::net::SocketAddr,
    id: usize,
    proxy: Arc<TcpProxy>,
    shutdown_flag: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let domain = if addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    tokio::spawn(async move {
        let sock = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
            .expect("create socket");
        sock.set_reuse_address(true).ok();
        #[cfg(target_os = "linux")]
        sock.set_reuse_port(true).ok();
        sock.set_nonblocking(true).ok();
        sock.bind(&socket2::SockAddr::from(addr))
            .unwrap_or_else(|e| panic!("bind acceptor {}: {}", id, e));
        sock.listen(1024)
            .unwrap_or_else(|e| panic!("listen acceptor {}: {}", id, e));
        let std_listener: std::net::TcpListener = sock.into();
        let listener = TcpListener::from_std(std_listener)
            .unwrap_or_else(|e| panic!("tokio listener acceptor {}: {}", id, e));

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (stream, peer_addr) = match accept_result {
                        Ok(s) => s,
                        Err(e) => {
                            error!("acceptor {} accept error: {}", id, e);
                            continue;
                        }
                    };
                    proxy.handle_accept(stream, peer_addr).await;
                }
                _ = async { while !shutdown_flag.load(Ordering::Acquire) {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                } } => {
                    info!("acceptor {} received shutdown signal", id);
                    break;
                }
            }
        }
    })
}

impl TcpProxy {
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
                metrics::record_connection_failed();
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
                metrics::record_connection_failed();
                return;
            }
        };
        metrics::record_handshake(handshake_start.elapsed());

        let mut upstream_stream = match upstream_result {
            Ok(s) => s,
            Err(e) => {
                warn!("upstream connect failed to {}: {}", upstream, e);
                metrics::record_connection_failed();
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
                metrics::record_connection_failed();
                return;
            }
        }

        let mut detect_buf = [0u8; buffers::PROTOCOL_DETECT];
        let mut tls_reader = tls_stream.inner;
        let detect_len = match tls_reader.read(&mut detect_buf).await {
            Ok(0) => {
                debug!("client {} closed before sending data", peer_id);
                metrics::record_connection_failed();
                return;
            }
            Ok(n) => n,
            Err(e) => {
                warn!("protocol detection read failed for {}: {}", peer_id, e);
                metrics::record_connection_failed();
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
            metrics::record_connection_failed();
            return;
        }

        let copy_result =
            crate::proxy::copy_bidirectional(&mut tls_reader, &mut upstream_stream).await;
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
