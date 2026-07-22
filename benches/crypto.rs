mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use interlink::common::error::InterlinkError;
use interlink::common::identity::{IdentityProvider, SpiffeId, TrustDomain};
use interlink::identity::ca::CertificateAuthority;
use interlink::proxy::handshake::{TlsClient, TlsServer};
use interlink::proxy::TlsHandshake;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone)]
struct Provider {
    identity: SpiffeId,
    trust_domain: TrustDomain,
}

impl IdentityProvider for Provider {
    fn get_identity(&self) -> Result<SpiffeId, InterlinkError> {
        Ok(self.identity.clone())
    }

    fn get_trust_domain(&self) -> &TrustDomain {
        &self.trust_domain
    }
}

struct TlsFixture {
    server: Arc<TlsServer>,
    wrong_domain_server: Arc<TlsServer>,
    client_identity: SpiffeId,
    client_cert: Vec<u8>,
    client_key: Vec<u8>,
    trusted_root: Vec<u8>,
}

impl TlsFixture {
    fn new() -> Self {
        let ca = CertificateAuthority::new("bench.local").unwrap();
        let server_identity = SpiffeId::try_new("bench.local", "default", "server").unwrap();
        let client_identity = SpiffeId::try_new("bench.local", "default", "client").unwrap();
        let (server_cert, server_key) = ca
            .issue_leaf_with_key(&server_identity, &["localhost"])
            .unwrap();
        let (client_cert, client_key) = ca
            .issue_leaf_with_key(&client_identity, &["localhost"])
            .unwrap();
        let trusted_root = ca.root_cert_der().to_vec();
        let trust_domain = TrustDomain::new("bench.local").with_ca(trusted_root.clone());
        let server = TlsServer::new(
            Arc::new(Provider {
                identity: server_identity,
                trust_domain,
            }),
            server_cert,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key)),
        )
        .unwrap();
        let wrong_identity = SpiffeId::try_new("other.local", "default", "server").unwrap();
        let (wrong_cert, wrong_key) = ca
            .issue_leaf_with_key(&wrong_identity, &["localhost"])
            .unwrap();
        let wrong_domain_server = TlsServer::new(
            Arc::new(Provider {
                identity: wrong_identity,
                trust_domain: TrustDomain::new("bench.local").with_ca(trusted_root.clone()),
            }),
            wrong_cert,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(wrong_key)),
        )
        .unwrap();

        Self {
            server: Arc::new(server),
            wrong_domain_server: Arc::new(wrong_domain_server),
            client_identity,
            client_cert: client_cert.as_ref().to_vec(),
            client_key,
            trusted_root,
        }
    }

    fn client(&self) -> TlsClient {
        self.client_trusting(self.trusted_root.clone())
    }

    fn client_trusting(&self, root: Vec<u8>) -> TlsClient {
        let trust_domain = TrustDomain::new("bench.local").with_ca(root);
        TlsClient::with_client_auth(
            Arc::new(Provider {
                identity: self.client_identity.clone(),
                trust_domain,
            }),
            CertificateDer::from(self.client_cert.clone()),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.client_key.clone())),
        )
        .unwrap()
    }

    fn unauthenticated_client(&self) -> TlsClient {
        TlsClient::new(Arc::new(Provider {
            identity: self.client_identity.clone(),
            trust_domain: TrustDomain::new("bench.local").with_ca(self.trusted_root.clone()),
        }))
        .unwrap()
    }
}

async fn spawn_server(
    server: Arc<TlsServer>,
    bind: SocketAddr,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(bind).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let server = server.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = server.accept(stream).await else {
                    return;
                };
                let mut byte = [0u8; 1];
                if tls.inner.read_exact(&mut byte).await.is_ok() {
                    let _ = tls.inner.write_all(&byte).await;
                    let _ = tls.inner.flush().await;
                }
            });
        }
    });
    (addr, task)
}

async fn exchange(
    client: &TlsClient,
    addr: SocketAddr,
) -> Result<Option<rustls::HandshakeKind>, InterlinkError> {
    let mut tls = client.connect(addr).await?;
    let kind = tls.inner.get_ref().1.handshake_kind();
    tls.inner
        .write_all(b"x")
        .await
        .map_err(InterlinkError::Io)?;
    tls.inner.flush().await.map_err(InterlinkError::Io)?;
    let mut byte = [0u8; 1];
    tls.inner
        .read_exact(&mut byte)
        .await
        .map_err(InterlinkError::Io)?;
    Ok(kind)
}

async fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
    (client.unwrap(), accepted.unwrap().0)
}

fn bench_certificate_paths(c: &mut Criterion) {
    let ca = CertificateAuthority::new("bench.local").unwrap();
    let identity = SpiffeId::try_new("bench.local", "default", "service").unwrap();

    let non_ascii = SpiffeId::try_new("bench.local", "default", "servicé").unwrap();
    let mut group = c.benchmark_group("certificate");
    group.bench_function("new_ca", |b| {
        b.iter(|| black_box(CertificateAuthority::new(black_box("bench.local")).unwrap()))
    });
    group.bench_function("issue_leaf", |b| {
        b.iter(|| black_box(ca.issue_leaf(black_box(&identity)).unwrap()))
    });
    group.bench_function("issue_leaf_with_dns_and_ip", |b| {
        b.iter(|| {
            black_box(
                ca.issue_leaf_with_key(
                    black_box(&identity),
                    black_box(&["localhost", "127.0.0.1"]),
                )
                .unwrap(),
            )
        })
    });
    group.bench_function("reject_non_ascii_identity", |b| {
        b.iter(|| {
            let result = ca.issue_leaf(black_box(&non_ascii));
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("reject_non_ascii_dns", |b| {
        b.iter(|| {
            let result = ca.issue_leaf_with_key(black_box(&identity), black_box(&["tést.local"]));
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("root_cert", |b| b.iter(|| black_box(ca.root_cert_der())));
    group.bench_function("root_key", |b| b.iter(|| black_box(ca.root_key())));
    group.finish();
}

fn bench_tls_paths(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let fixture = TlsFixture::new();
    let (ipv4_addr, ipv4_server) = runtime.block_on(spawn_server(
        fixture.server.clone(),
        SocketAddr::from(([127, 0, 0, 1], 0)),
    ));
    let (ipv6_addr, ipv6_server) = runtime.block_on(spawn_server(
        fixture.server.clone(),
        "[::1]:0".parse().unwrap(),
    ));
    let (wrong_domain_addr, wrong_domain_task) = runtime.block_on(spawn_server(
        fixture.wrong_domain_server.clone(),
        SocketAddr::from(([127, 0, 0, 1], 0)),
    ));

    let resumed_client = fixture.client();
    runtime
        .block_on(exchange(&resumed_client, ipv4_addr))
        .unwrap();
    assert_eq!(
        runtime
            .block_on(exchange(&resumed_client, ipv4_addr))
            .unwrap(),
        Some(rustls::HandshakeKind::Resumed),
        "benchmark fixture must positively verify TLS resumption"
    );

    let ipv6_client = fixture.client();
    runtime.block_on(exchange(&ipv6_client, ipv6_addr)).unwrap();

    let untrusted_ca = CertificateAuthority::new("bench.local").unwrap();
    let rejecting_client = fixture.client_trusting(untrusted_ca.root_cert_der().to_vec());

    let no_auth_client = fixture.unauthenticated_client();
    let (client_side, accepted_side) = runtime.block_on(tcp_pair());

    let mut config_group = c.benchmark_group("tls_config");
    config_group.bench_function("client_no_auth", |b| {
        b.iter(|| black_box(fixture.unauthenticated_client()))
    });
    config_group.bench_function("client_rejects_accept", |b| {
        b.to_async(&runtime).iter(|| async {
            let (_client, accepted) = tcp_pair().await;
            let result = no_auth_client.accept(accepted).await;
            assert!(result.is_err());
            black_box(result)
        })
    });
    config_group.bench_function("server_rejects_connect", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = fixture.server.connect(ipv4_addr).await;
            assert!(result.is_err());
            black_box(result)
        })
    });
    config_group.finish();
    drop(client_side);
    drop(accepted_side);

    let mut group = c.benchmark_group("tls_handshake");
    group.bench_function("full", |b| {
        b.to_async(&runtime).iter(|| {
            let client = fixture.client();
            async move {
                let kind = exchange(&client, ipv4_addr).await.unwrap();
                assert_eq!(kind, Some(rustls::HandshakeKind::Full));
                black_box(kind)
            }
        })
    });
    group.bench_function("resumed", |b| {
        b.to_async(&runtime).iter(|| async {
            let kind = exchange(&resumed_client, ipv4_addr).await.unwrap();
            assert_eq!(kind, Some(rustls::HandshakeKind::Resumed));
            black_box(kind)
        })
    });
    group.bench_function("ipv6", |b| {
        b.to_async(&runtime)
            .iter(|| async { black_box(exchange(&ipv6_client, ipv6_addr).await.unwrap()) })
    });
    group.bench_function("reject_untrusted_ca", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = rejecting_client.connect(ipv4_addr).await;
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.bench_function("reject_wrong_trust_domain", |b| {
        b.to_async(&runtime).iter(|| async {
            let client = fixture.client();
            let result = client.connect(wrong_domain_addr).await;
            assert!(result.is_err());
            black_box(result)
        })
    });
    group.finish();

    ipv4_server.abort();
    ipv6_server.abort();
    wrong_domain_task.abort();
}

criterion_group!(
    name = benches;
    config = support::criterion();
    targets = bench_certificate_paths, bench_tls_paths
);
criterion_main!(benches);
