#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
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
use crate::proxy::UpstreamTarget;

pub struct TcpProxy {
    default_upstream: Option<UpstreamTarget>,
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
        let default_upstream = UpstreamTarget::from_config(config.default_upstream);
        Self {
            default_upstream,
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

        // Port 0 disables this proxy (outbound-only sidecars), symmetric with
        // the outbound proxy (C3). Without the guard we would bind an
        // ephemeral port and serve mTLS on it unintentionally.
        if self.listen_port == 0 {
            info!("inbound proxy disabled (port 0)");
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
                Err(e) => error!("failed to bind acceptor {} on {}: {}", i, addr, e),
            }
        }
        if listeners.is_empty() {
            error!("no acceptors could bind {}; proxy not started", addr);
            return;
        }
        info!(
            "interlink proxy listening on {} with {} acceptors",
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
                                        error!("acceptor {} accept error: {}", id, e);
                                        continue;
                                    }
                                };
                                proxy.handle_accept(stream, peer_addr).await;
                            }
                            _ = crate::proxy::wait_shutdown(shutdown.clone()) => {
                                info!("acceptor {} received shutdown signal", id);
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

impl TcpProxy {
    async fn handle_accept(self: &Arc<Self>, stream: TcpStream, peer_addr: std::net::SocketAddr) {
        if let Err(e) = configure_socket(&stream) {
            warn!("failed to configure accepted socket {}: {}", peer_addr, e);
        }

        let upstream = self
            .default_upstream
            .clone()
            .or_else(|| get_original_dst(&stream).map(UpstreamTarget::Socket));
        let Some(upstream) = upstream else {
            warn!("no upstream for connection from {}, dropping", peer_addr);
            let _ = stream.into_std().map(|s| {
                let _ = s.shutdown(std::net::Shutdown::Both);
            });
            return;
        };

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
        upstream: UpstreamTarget,
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

        // Mux tunnel path (ALPN-negotiated): the whole connection is a yamux
        // session; every stream relays to this connection's upstream. The
        // pre-connected upstream socket belongs to the 1:1 model — release it
        // and let each stream dial its own.
        if crate::proxy::mux::negotiated_alpn(&tls_stream.inner)
            == Some(crate::proxy::mux::ALPN_MUX)
        {
            let peer = tls_stream.peer_identity.clone();
            let _ = upstream_stream
                .into_std()
                .map(|s| s.shutdown(std::net::Shutdown::Both));
            // The tunnel holds this accepted connection's active slot for its
            // lifetime; streams are the accounted connections (serve_tunnel).
            crate::proxy::mux::serve_tunnel(tls_stream.inner, upstream, peer).await;
            metrics::record_tunnel_closed();
            return;
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

        if let Err(e) = crate::proxy::record_relay_result(
            crate::proxy::copy_bidirectional(&mut tls_reader, &mut upstream_stream).await,
            start,
        ) {
            warn!(
                "bidirectional copy error for {} → {}: {}",
                peer_id, upstream, e
            );
        }
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
