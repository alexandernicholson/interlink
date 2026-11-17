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
use crate::proxy::config::{EgressPolicy, ProxyConfig};
use crate::proxy::configure_socket;
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
    egress: EgressPolicy,
    dialer: Arc<dyn crate::proxy::UpstreamDialer>,
    tunnel_max_age: std::time::Duration,
    max_streams_per_tunnel: usize,
    total_streams: Arc<Semaphore>,
    original_destination: Arc<dyn crate::proxy::OriginalDestination>,
}

impl TcpProxy {
    pub fn new(
        config: ProxyConfig,
        tls_server: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
    ) -> Result<Self, InterlinkError> {
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
    ) -> Result<Self, InterlinkError> {
        Self::new_with_discovery(config, port, tls_server, policy, None)
    }

    pub fn new_with_discovery(
        config: ProxyConfig,
        port: u16,
        tls_server: Arc<dyn TlsHandshake>,
        policy: Arc<PolicyEngine>,
        discovery: Option<Arc<ServiceDiscovery>>,
    ) -> Result<Self, InterlinkError> {
        let max_conn = config
            .max_connections
            .unwrap_or(crate::common::constants::limits::MAX_CONNECTIONS);
        let local_id = config.local_identity()?;
        let egress = EgressPolicy::parse(&config.egress_subnets)?;
        if config.tunnel_max_age_secs == 0 || config.mux_max_streams_per_tunnel == 0 {
            return Err(InterlinkError::Config(
                "tunnel limits must be positive".into(),
            ));
        }
        let default_upstream = UpstreamTarget::from_config(config.default_upstream);
        Ok(Self {
            default_upstream,
            connection_semaphore: Arc::new(Semaphore::new(max_conn)),
            listen_port: port,
            tls_server,
            policy,
            discovery,
            shutdown: None,
            active_connections: Arc::new(AtomicUsize::new(0)),
            local_id,
            egress,
            dialer: Arc::new(crate::proxy::TcpDialer),
            tunnel_max_age: std::time::Duration::from_secs(config.tunnel_max_age_secs),
            max_streams_per_tunnel: config.mux_max_streams_per_tunnel,
            // max_conn permits shared by every tunnel of this proxy: mux
            // streams are admitted connections, like legacy relays. Each mux
            // relay holds one named guard and releases it on exit.
            total_streams: Arc::new(Semaphore::new(max_conn)),
            original_destination: Arc::new(crate::proxy::NetfilterDestination),
        })
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
                let mut acceptor =
                    crate::proxy::Acceptor::new(listener, "acceptor", id, self.shutdown.clone());
                let proxy = self.clone();
                tokio::spawn(async move {
                    while let Some((stream, peer_addr)) = acceptor.next().await {
                        proxy.handle_accept(stream, peer_addr).await;
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

        let upstream = self.default_upstream.clone().or_else(|| {
            self.original_destination
                .get(&stream)
                .filter(|sa| sa.port() != self.listen_port)
                .map(UpstreamTarget::Socket)
        });
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

        // Authentication and policy must complete before any upstream I/O:
        // unauthenticated clients must not turn this listener into a dial oracle.
        let handshake_start = Instant::now();
        let tls_stream = match self.tls_server.accept(stream).await {
            Ok(s) => s,
            Err(e) => {
                warn!("mTLS handshake failed from {}: {}", peer_addr, e);
                metrics::record_handshake_error();
                metrics::record_connection_failed();
                return;
            }
        };
        metrics::record_handshake(handshake_start.elapsed());

        let peer_id = &tls_stream.peer_identity;
        debug!(
            "mTLS connection from {} identity={}",
            peer_addr,
            peer_id.sanitized()
        );

        let decision = self.policy.evaluate(peer_id, &self.local_id);
        metrics::record_policy(&decision);
        match decision {
            Decision::Allow => {}
            Decision::Deny(reason) => {
                warn!(
                    "policy denied {} → {}: {}",
                    peer_id.sanitized(),
                    self.local_id.sanitized(),
                    reason
                );
                metrics::record_connection_failed();
                return;
            }
        }

        let upstream = match self.resolve_upstream(&upstream).await {
            Ok(addr) => addr,
            Err(e) => {
                warn!("failed to resolve upstream {}: {}", upstream, e);
                metrics::record_connection_failed();
                return;
            }
        };
        if !self.egress.allows(upstream.ip()) {
            warn!("egress denied {} → {}", self.local_id.sanitized(), upstream);
            metrics::record_policy(&Decision::Deny(
                "egress destination outside allowed subnets",
            ));
            metrics::record_connection_failed();
            return;
        }

        // A negotiated mux tunnel dials only when its authorized streams open.
        if crate::proxy::mux::negotiated_alpn(&tls_stream.inner)
            == Some(crate::proxy::mux::ALPN_MUX)
        {
            let limits = crate::proxy::mux::TunnelLimits {
                max_age: self.tunnel_max_age,
                max_streams_per_tunnel: self.max_streams_per_tunnel,
                total_streams: self.total_streams.clone(),
            };
            if let Err(e) = crate::proxy::mux::serve_tunnel(
                tls_stream.inner,
                upstream,
                peer_id.as_ref().clone(),
                self.local_id.clone(),
                self.policy.clone(),
                limits,
            )
            .await
            {
                warn!("mux tunnel failed for {}: {}", peer_id.sanitized(), e);
            }
            metrics::record_tunnel_closed();
            return;
        }

        let mut upstream_stream = match self.dialer.connect(upstream).await {
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

#[cfg(test)]
mod security_tests {
    use super::*;
    use crate::proxy::test_support::*;

    fn proxy(
        config: ProxyConfig,
        port: u16,
        handshake: Arc<dyn TlsHandshake>,
        dialer: Arc<GateDialer>,
        allowed: bool,
    ) -> Arc<TcpProxy> {
        let mut policy = PolicyEngine::new();
        if allowed {
            policy.set_default_decision(Decision::Allow);
        }
        let mut proxy = TcpProxy::new_with_port(config, port, handshake, Arc::new(policy)).unwrap();
        proxy.dialer = dialer;
        proxy.original_destination = Arc::new(DirectDestination);
        Arc::new(proxy)
    }

    #[tokio::test]
    async fn inbound_direct_listener_drops_without_permit_or_dial() {
        tokio::time::timeout(WAIT, async {
            let (handshake, _) = handshake(false);
            let dialer = GateDialer::new();
            let (mut client, server, peer) = pair().await;
            let port = server.local_addr().unwrap().port();
            let proxy = proxy(config(), port, handshake.clone(), dialer.clone(), true);
            proxy.handle_accept(server, peer).await;
            assert_eq!(client.read(&mut [0; 1]).await.unwrap(), 0);
            assert_eq!(proxy.connection_semaphore.available_permits(), 4);
            assert_eq!(proxy.active_connections.load(Ordering::SeqCst), 0);
            assert_eq!(handshake.calls.load(Ordering::SeqCst), 0);
            assert_eq!(dialer.calls.load(Ordering::SeqCst), 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn concurrent_handshake_failures_release_permits_without_dials() {
        tokio::time::timeout(WAIT, async {
            let (handshake, _) = handshake(false);
            let dialer = GateDialer::new();
            let mut config = config();
            config.default_upstream = Some("127.0.0.1:9".into());
            let proxy = proxy(config, 4143, handshake.clone(), dialer.clone(), true);
            let mut clients = Vec::new();
            for _ in 0..4 {
                let (client, server, peer) = pair().await;
                proxy.handle_accept(server, peer).await;
                clients.push(client);
            }
            wait_for(|| handshake.calls.load(Ordering::SeqCst) == 4).await;
            assert_eq!(proxy.connection_semaphore.available_permits(), 0);
            assert_eq!(dialer.calls.load(Ordering::SeqCst), 0);
            handshake.release.add_permits(4);
            wait_for(|| proxy.connection_semaphore.available_permits() == 4).await;
            assert_eq!(dialer.calls.load(Ordering::SeqCst), 0);
            assert_eq!(proxy.active_connections.load(Ordering::SeqCst), 0);
            for mut client in clients {
                assert_eq!(client.read(&mut [0; 1]).await.unwrap(), 0);
            }
        })
        .await
        .unwrap();
    }

    async fn authenticated_flow(allow_policy: bool, subnets: Vec<String>, expect_dial: bool) {
        tokio::time::timeout(WAIT, async {
            let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (handshake, client_config) = handshake(true);
            let dialer = GateDialer::new();
            let mut config = config();
            config.default_upstream = Some(upstream.local_addr().unwrap().to_string());
            config.egress_subnets = subnets;
            let proxy = proxy(
                config,
                4143,
                handshake.clone(),
                dialer.clone(),
                allow_policy,
            );
            let (client, server, peer) = pair().await;
            proxy.handle_accept(server, peer).await;
            wait_for(|| handshake.calls.load(Ordering::SeqCst) == 1).await;
            // The handshake is held in-flight: upstream dialing must not start.
            assert_eq!(dialer.calls.load(Ordering::SeqCst), 0);
            handshake.release.add_permits(1);
            let mut client = tokio_rustls::TlsConnector::from(client_config)
                .connect(
                    rustls::pki_types::ServerName::try_from("localhost").unwrap(),
                    client,
                )
                .await
                .unwrap();
            if expect_dial {
                wait_for(|| dialer.calls.load(Ordering::SeqCst) == 1).await;
                assert_eq!(proxy.connection_semaphore.available_permits(), 3);
                dialer.release.add_permits(1);
                let (mut backend, _) = upstream.accept().await.unwrap();
                client.write_all(b"hello").await.unwrap();
                client.shutdown().await.unwrap();
                let mut received = Vec::new();
                backend.read_to_end(&mut received).await.unwrap();
                assert_eq!(received, b"hello");
                backend.write_all(b"reply").await.unwrap();
                backend.shutdown().await.unwrap();
                let mut reply = Vec::new();
                client.read_to_end(&mut reply).await.unwrap();
                assert_eq!(reply, b"reply");
            } else {
                // Policy/egress denial must return even while the dialer's gate
                // is closed; a regression would hold this permit until timeout.
                wait_for(|| proxy.connection_semaphore.available_permits() == 4).await;
                assert_eq!(dialer.calls.load(Ordering::SeqCst), 0);
            }
            wait_for(|| proxy.connection_semaphore.available_permits() == 4).await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn inbound_policy_denial_never_dials() {
        authenticated_flow(false, Vec::new(), false).await;
    }

    #[tokio::test]
    async fn inbound_egress_denial_never_dials() {
        authenticated_flow(true, vec!["10.0.0.0/8".into()], false).await;
    }

    #[tokio::test]
    async fn inbound_authorizes_before_dial_and_relays_allowed_subnet() {
        authenticated_flow(true, vec!["127.0.0.0/8".into()], true).await;
    }

    #[tokio::test]
    async fn inbound_empty_egress_allows_authenticated_relay() {
        authenticated_flow(true, Vec::new(), true).await;
    }

    #[test]
    fn inbound_constructors_reject_missing_invalid_identity_and_cidr() {
        let (handshake, _) = handshake(false);
        let policy = Arc::new(PolicyEngine::new());
        for identity in [None, Some("not-spiffe".into())] {
            let mut config = config();
            config.identity = identity;
            assert!(TcpProxy::new(config.clone(), handshake.clone(), policy.clone()).is_err());
            assert!(TcpProxy::new_with_port(
                config.clone(),
                4143,
                handshake.clone(),
                policy.clone()
            )
            .is_err());
            assert!(TcpProxy::new_with_discovery(
                config,
                4143,
                handshake.clone(),
                policy.clone(),
                None
            )
            .is_err());
        }
        for cidr in ["", "127.0.0.1", "127.0.0.1/33", "::1/129", "not-a-cidr"] {
            let mut config = config();
            config.egress_subnets = vec![cidr.into()];
            let result = TcpProxy::new(config, handshake.clone(), policy.clone());
            let error = result.err().expect("malformed CIDR must fail").to_string();
            assert!(error.contains(&format!("{cidr:?}")), "{error}");
        }
        assert!(TcpProxy::new(config(), handshake, policy).is_ok());
    }

    #[test]
    fn inbound_tunnel_limits_are_validated_and_shared_per_proxy() {
        let (handshake, _) = handshake(false);
        let policy = Arc::new(PolicyEngine::new());
        for (age, streams) in [(0, 64), (3600, 0)] {
            let mut config = config();
            config.tunnel_max_age_secs = age;
            config.mux_max_streams_per_tunnel = streams;
            assert!(TcpProxy::new(config, handshake.clone(), policy.clone()).is_err());
        }
        let mut config = config();
        config.tunnel_max_age_secs = 17;
        config.mux_max_streams_per_tunnel = 3;
        config.max_connections = Some(4321);
        let proxy = TcpProxy::new(config, handshake, policy).unwrap();
        assert_eq!(proxy.tunnel_max_age, std::time::Duration::from_secs(17));
        assert_eq!(proxy.max_streams_per_tunnel, 3);
        assert_eq!(
            proxy.total_streams.available_permits(),
            4321,
            "mux streams share the configured admission limit"
        );
    }
}

#[cfg(test)]
mod resolution_tests {
    use super::*;
    use crate::proxy::test_support::*;

    #[tokio::test]
    async fn inbound_hostname_endpoint_keeps_explicit_port_for_discovery() {
        let (handshake, _) = handshake(false);
        let discovery = Arc::new(ServiceDiscovery::with_resolver(
            Arc::new(ExpectedEndpoint),
            std::time::Duration::from_secs(30),
        ));
        let proxy = TcpProxy::new_with_discovery(
            config(),
            4143,
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
}
