//! Black-box counterparts to the gated proxy unit regressions (A1/D4).
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use interlink::common::error::InterlinkError;
use interlink::common::identity::{IdentityProvider, SpiffeId, TrustDomain};
use interlink::policy::{Decision, PolicyEngine};
use interlink::proxy::config::ProxyConfig;
use interlink::proxy::handshake::legacy_alpn_protocols;
use interlink::proxy::{OutboundProxy, TcpProxy, TlsClient, TlsHandshake, TlsServer};
use interlink_test_support::CertificateAuthority;
use rustls::pki_types::PrivatePkcs8KeyDer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

const WAIT: Duration = Duration::from_secs(3);

struct Provider {
    id: SpiffeId,
    trust: TrustDomain,
}

impl IdentityProvider for Provider {
    fn get_identity(&self) -> Result<SpiffeId, InterlinkError> {
        Ok(self.id.clone())
    }
    fn get_trust_domain(&self) -> &TrustDomain {
        &self.trust
    }
}

struct Credentials {
    server: Arc<TlsServer>,
    client: Arc<TlsClient>,
    id: SpiffeId,
}

fn credentials() -> Credentials {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let domain = "proxy-security.test";
    let ca = CertificateAuthority::new(domain).unwrap();
    let id = SpiffeId::try_new(domain, "default", "server").unwrap();
    let client_id = SpiffeId::try_new(domain, "default", "client").unwrap();
    let trust = TrustDomain::new(domain).with_ca(ca.root_cert_der().to_vec());
    let (server_cert, server_key) = ca.issue_leaf_with_key(&id, &["127.0.0.1"]).unwrap();
    let (client_cert, client_key) = ca.issue_leaf_with_key(&client_id, &["127.0.0.1"]).unwrap();
    let server = TlsServer::new_with_alpn(
        Arc::new(Provider {
            id: id.clone(),
            trust: trust.clone(),
        }),
        server_cert,
        PrivatePkcs8KeyDer::from(server_key).into(),
        legacy_alpn_protocols(),
    )
    .unwrap();
    let client = TlsClient::with_client_auth_alpn(
        Arc::new(Provider {
            id: client_id,
            trust,
        }),
        client_cert,
        PrivatePkcs8KeyDer::from(client_key).into(),
        legacy_alpn_protocols(),
    )
    .unwrap();
    Credentials {
        server: Arc::new(server),
        client: Arc::new(client),
        id,
    }
}

fn config(credentials: &Credentials) -> ProxyConfig {
    ProxyConfig {
        identity: Some(credentials.id.to_uri()),
        max_connections: Some(4),
        mux: false,
        ..Default::default()
    }
}

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn connect_ready(addr: SocketAddr) -> TcpStream {
    tokio::time::timeout(WAIT, async {
        loop {
            match tokio::time::timeout(WAIT, TcpStream::connect(addr))
                .await
                .unwrap()
            {
                Ok(stream) => return stream,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("connect {addr}: {error}"),
            }
        }
    })
    .await
    .unwrap()
}

async fn stop(shutdown: watch::Sender<bool>, task: tokio::task::JoinHandle<()>) {
    shutdown.send(true).unwrap();
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
}

async fn no_upstream_dial(upstream: &TcpListener) {
    assert!(
        tokio::time::timeout(Duration::from_millis(100), upstream.accept())
            .await
            .is_err(),
        "rejected connection must not create any upstream TCP connection"
    );
}

#[tokio::test]
async fn inbound_direct_connect_is_dropped_without_redirect() {
    tokio::time::timeout(WAIT, async {
        let credentials = credentials();
        let port = reserve_port();
        let (shutdown, rx) = watch::channel(false);
        let proxy = TcpProxy::new_with_port(
            config(&credentials),
            port,
            credentials.server,
            Arc::new(PolicyEngine::new()),
        )
        .unwrap()
        .with_shutdown(rx);
        let task = Arc::new(proxy).spawn();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        for _ in 0..2 {
            let mut stream = connect_ready(addr).await;
            assert_eq!(stream.read(&mut [0; 1]).await.unwrap(), 0);
        }
        stop(shutdown, task).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn outbound_loopback_bind_rejects_nonloopback_interface() {
    tokio::time::timeout(WAIT, async {
        // Validate the negative probe against a wildcard listener on the local
        // routed interface. UDP connect selects an interface without sending.
        let route = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        route.connect("192.0.2.1:9").unwrap();
        let alternate = route.local_addr().unwrap().ip();
        assert!(!alternate.is_loopback() && !alternate.is_unspecified());
        let control = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let probe = connect_ready(SocketAddr::new(
            alternate,
            control.local_addr().unwrap().port(),
        ))
        .await;
        drop(probe);
        drop(control);
        let credentials = credentials();
        let port = reserve_port();
        let (shutdown, rx) = watch::channel(false);
        let proxy = OutboundProxy::new_with_port(
            config(&credentials),
            port,
            credentials.client,
            Arc::new(PolicyEngine::new()),
        )
        .unwrap()
        .with_shutdown(rx);
        let task = Arc::new(proxy).spawn();
        drop(connect_ready(SocketAddr::from(([127, 0, 0, 1], port))).await);
        assert!(
            tokio::time::timeout(WAIT, TcpStream::connect(SocketAddr::new(alternate, port)))
                .await
                .unwrap()
                .is_err()
        );
        stop(shutdown, task).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inbound_failed_real_handshake_creates_no_upstream_connection() {
    tokio::time::timeout(WAIT, async {
        let credentials = credentials();
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = config(&credentials);
        config.default_upstream = Some(upstream.local_addr().unwrap().to_string());
        let mut policy = PolicyEngine::new();
        policy.set_default_decision(Decision::Allow);
        let port = reserve_port();
        let (shutdown, rx) = watch::channel(false);
        let proxy = TcpProxy::new_with_port(config, port, credentials.server, Arc::new(policy))
            .unwrap()
            .with_shutdown(rx);
        let task = Arc::new(proxy).spawn();
        let mut stream = connect_ready(SocketAddr::from(([127, 0, 0, 1], port))).await;
        stream.write_all(b"not a TLS ClientHello").await.unwrap();
        stream.shutdown().await.unwrap();
        // TLS may send a fatal alert before closing; drain it to observe EOF.
        let _closed = stream.read_to_end(&mut Vec::new()).await;
        no_upstream_dial(&upstream).await;
        stop(shutdown, task).await;
    })
    .await
    .unwrap();
}

async fn authenticated_egress(
    policy_allowed: bool,
    allowed_subnets: Vec<String>,
    expect_dial: bool,
) {
    tokio::time::timeout(WAIT, async {
        let credentials = credentials();
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = config(&credentials);
        config.default_upstream = Some(upstream.local_addr().unwrap().to_string());
        config.egress_subnets = allowed_subnets;
        let mut policy = PolicyEngine::new();
        if policy_allowed {
            policy.set_default_decision(Decision::Allow);
        }
        let port = reserve_port();
        let (shutdown, rx) = watch::channel(false);
        let proxy = TcpProxy::new_with_port(config, port, credentials.server, Arc::new(policy))
            .unwrap()
            .with_shutdown(rx);
        let task = Arc::new(proxy).spawn();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        drop(connect_ready(addr).await);
        let mut tls = credentials.client.connect(addr).await.unwrap();
        if expect_dial {
            let (mut backend, _) = upstream.accept().await.unwrap();
            tls.inner.write_all(b"allowed").await.unwrap();
            tls.inner.shutdown().await.unwrap();
            let mut received = Vec::new();
            backend.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, b"allowed");
            backend.shutdown().await.unwrap();
            let mut reply = Vec::new();
            tls.inner.read_to_end(&mut reply).await.unwrap();
        } else {
            assert!(matches!(tls.inner.read(&mut [0; 1]).await, Ok(0) | Err(_)));
            no_upstream_dial(&upstream).await;
        }
        stop(shutdown, task).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn inbound_real_policy_denial_creates_no_upstream_connection() {
    authenticated_egress(false, Vec::new(), false).await;
}

#[tokio::test]
async fn inbound_real_egress_denial_creates_no_upstream_connection() {
    authenticated_egress(true, vec!["192.0.2.0/24".into()], false).await;
}

#[tokio::test]
async fn inbound_real_allowed_egress_relays_after_authentication() {
    authenticated_egress(true, vec!["127.0.0.0/8".into()], true).await;
}
