mod support;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use interlink::common::error::InterlinkError;
use interlink::common::identity::{IdentityProvider, SpiffeId, TrustDomain};
use interlink::identity::ca::CertificateAuthority;
use interlink::policy::{patterns, PolicyEngine};
use interlink::proxy::config::ProxyConfig;
use interlink::proxy::handshake::{
    legacy_alpn_protocols, TlsClient, TlsHandshake, TlsServer, TlsStream,
};
use interlink::proxy::{OutboundProxy, TcpProxy};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Notify, Semaphore};

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

struct FailingHandshake;

#[async_trait]
impl TlsHandshake for FailingHandshake {
    async fn connect(&self, _addr: SocketAddr) -> Result<TlsStream, InterlinkError> {
        Err(InterlinkError::Tls(rustls::Error::General(
            "injected handshake failure".into(),
        )))
    }

    async fn accept(&self, _stream: TcpStream) -> Result<TlsStream, InterlinkError> {
        Err(InterlinkError::Tls(rustls::Error::General(
            "injected handshake failure".into(),
        )))
    }
}

struct BlockingHandshake {
    entered: Notify,
    release: Semaphore,
    completed: Notify,
}

impl BlockingHandshake {
    fn new() -> Self {
        Self {
            entered: Notify::new(),
            release: Semaphore::new(0),
            completed: Notify::new(),
        }
    }

    fn release_one(&self) {
        self.release.add_permits(1);
    }
}

#[async_trait]
impl TlsHandshake for BlockingHandshake {
    async fn connect(&self, _addr: SocketAddr) -> Result<TlsStream, InterlinkError> {
        self.entered.notify_one();
        // Invariant: the semaphore starts at zero; the harness adds exactly
        // one permit for each entered call; forgetting consumes that token so
        // every subsequent call blocks until explicitly released.
        let release_permit = self.release.acquire().await.map_err(|_| {
            InterlinkError::Tls(rustls::Error::General(
                "benchmark release semaphore closed".into(),
            ))
        })?;
        release_permit.forget();
        self.completed.notify_one();
        Err(InterlinkError::Tls(rustls::Error::General(
            "released benchmark handshake".into(),
        )))
    }

    async fn accept(&self, _stream: TcpStream) -> Result<TlsStream, InterlinkError> {
        Err(InterlinkError::Tls(rustls::Error::General(
            "blocking client cannot accept".into(),
        )))
    }
}

fn reserve_port() -> u16 {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.local_addr().unwrap().port()
}

#[derive(Clone, Copy)]
enum BackendMode {
    Echo,
    ReplyAfterEof,
    Reset,
}

struct ProxyHarness {
    outbound_addr: SocketAddr,
    shutdown: Vec<watch::Sender<bool>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ProxyHarness {
    async fn start(mux: bool, allow: bool) -> Self {
        Self::start_with_backend(mux, allow, BackendMode::Echo).await
    }

    async fn start_with_backend(mux: bool, allow: bool, backend_mode: BackendMode) -> Self {
        let echo_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let echo_addr = echo_listener.local_addr().unwrap();
        let echo_task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = echo_listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    match backend_mode {
                        BackendMode::Echo => {
                            let mut buffer = [0u8; 16 * 1024];
                            loop {
                                match stream.read(&mut buffer).await {
                                    Ok(0) | Err(_) => return,
                                    Ok(n) => {
                                        if stream.write_all(&buffer[..n]).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        BackendMode::ReplyAfterEof => {
                            let mut request = Vec::new();
                            if stream.read_to_end(&mut request).await.is_ok() {
                                let _ = stream.write_all(b"half-close-ok").await;
                            }
                        }
                        BackendMode::Reset => {
                            let socket = socket2::SockRef::from(&stream);
                            let _ = socket.set_linger(Some(Duration::ZERO));
                        }
                    }
                });
            }
        });

        let inbound_port = reserve_port();
        let outbound_port = reserve_port();
        let trust_domain_name = "bench-mesh.local";
        let ca = CertificateAuthority::new(trust_domain_name).unwrap();
        let server_identity = SpiffeId::try_new(trust_domain_name, "default", "inbound").unwrap();
        let client_identity = SpiffeId::try_new(trust_domain_name, "default", "outbound").unwrap();
        let (server_cert, server_key) = ca
            .issue_leaf_with_key(&server_identity, &["localhost"])
            .unwrap();
        let (client_cert, client_key) = ca
            .issue_leaf_with_key(&client_identity, &["localhost"])
            .unwrap();
        let trust_domain = TrustDomain::new(trust_domain_name).with_ca(ca.root_cert_der().to_vec());

        let server = Arc::new(
            TlsServer::new(
                Arc::new(Provider {
                    identity: server_identity.clone(),
                    trust_domain: trust_domain.clone(),
                }),
                server_cert,
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key)),
            )
            .unwrap(),
        );
        let client_provider = Arc::new(Provider {
            identity: client_identity.clone(),
            trust_domain: trust_domain.clone(),
        });
        let client = Arc::new(if mux {
            TlsClient::with_client_auth(
                client_provider,
                client_cert,
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client_key)),
            )
            .unwrap()
        } else {
            TlsClient::with_client_auth_alpn(
                client_provider,
                client_cert,
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client_key)),
                legacy_alpn_protocols(),
            )
            .unwrap()
        });

        let policy = Arc::new(PolicyEngine::new());
        if allow {
            policy.add_namespace_rule(
                "default",
                patterns::allow_same_namespace(trust_domain_name, "default"),
            );
        }

        let (inbound_shutdown_tx, inbound_shutdown_rx) = watch::channel(false);
        let inbound = Arc::new(
            TcpProxy::new_with_port(
                ProxyConfig {
                    trust_domain: trust_domain_name.into(),
                    identity: Some(server_identity.to_uri()),
                    default_upstream: Some(echo_addr.to_string()),
                    max_connections: Some(1024),
                    mux,
                },
                inbound_port,
                server,
                policy.clone(),
            )
            .with_shutdown(inbound_shutdown_rx),
        );

        let (outbound_shutdown_tx, outbound_shutdown_rx) = watch::channel(false);
        let outbound = Arc::new(
            OutboundProxy::new_with_port(
                ProxyConfig {
                    trust_domain: trust_domain_name.into(),
                    identity: Some(client_identity.to_uri()),
                    default_upstream: Some(
                        SocketAddr::from(([127, 0, 0, 1], inbound_port)).to_string(),
                    ),
                    max_connections: Some(1024),
                    mux,
                },
                outbound_port,
                client,
                policy,
            )
            .with_shutdown(outbound_shutdown_rx),
        );

        let inbound_task = inbound.spawn();
        let outbound_task = outbound.spawn();
        let outbound_addr = SocketAddr::from(([127, 0, 0, 1], outbound_port));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if TcpStream::connect(outbound_addr).await.is_ok() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        Self {
            outbound_addr,
            shutdown: vec![inbound_shutdown_tx, outbound_shutdown_tx],
            tasks: vec![echo_task, inbound_task, outbound_task],
        }
    }

    async fn request(&self, payload: &'static [u8]) -> std::io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect(self.outbound_addr).await?;
        stream.write_all(payload).await?;
        let mut response = vec![0u8; payload.len()];
        stream.read_exact(&mut response).await?;
        Ok(response)
    }

    async fn denied(&self) -> std::io::Result<usize> {
        let mut stream = TcpStream::connect(self.outbound_addr).await?;
        stream.write_all(b"denied").await?;
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_millis(100),
            stream.read_to_end(&mut response),
        )
        .await
        .map_err(std::io::Error::other)??;
        Ok(response.len())
    }

    async fn half_close(&self) -> std::io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect(self.outbound_addr).await?;
        stream.write_all(b"request-until-eof").await?;
        stream.shutdown().await?;
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_millis(100),
            stream.read_to_end(&mut response),
        )
        .await
        .map_err(std::io::Error::other)??;
        Ok(response)
    }

    fn stop(&self) {
        for shutdown in &self.shutdown {
            let _ = shutdown.send(true);
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct OutboundHarness {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl OutboundHarness {
    async fn start(
        default_upstream: Option<String>,
        max_connections: usize,
        handshake: Arc<dyn TlsHandshake>,
    ) -> Self {
        let port = reserve_port();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let proxy = Arc::new(
            OutboundProxy::new_with_port(
                ProxyConfig {
                    trust_domain: "bench-failure.local".into(),
                    identity: Some("spiffe://bench-failure.local/ns/default/sa/outbound".into()),
                    default_upstream,
                    max_connections: Some(max_connections),
                    mux: false,
                },
                port,
                handshake,
                Arc::new(PolicyEngine::new()),
            )
            .with_shutdown(shutdown_rx),
        );
        let task = proxy.spawn();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if TcpStream::connect(addr).await.is_ok() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        Self {
            addr,
            shutdown,
            task,
        }
    }

    async fn closed(&self) -> std::io::Result<usize> {
        let mut stream = TcpStream::connect(self.addr).await?;
        let _ = stream.write_all(b"closed").await;
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_millis(100),
            stream.read_to_end(&mut response),
        )
        .await
        .map_err(std::io::Error::other)??;
        Ok(response.len())
    }

    async fn saturate(&self, handshake: &BlockingHandshake) -> std::io::Result<usize> {
        let mut first = TcpStream::connect(self.addr).await?;
        let _ = first.write_all(b"hold").await;
        tokio::time::timeout(Duration::from_millis(100), handshake.entered.notified())
            .await
            .map_err(std::io::Error::other)?;

        let mut second = TcpStream::connect(self.addr).await?;
        let _ = second.write_all(b"reject").await;
        let mut response = Vec::new();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            second.read_to_end(&mut response),
        )
        .await
        .map_err(std::io::Error::other)
        .and_then(|result| result);

        handshake.release_one();
        tokio::time::timeout(Duration::from_millis(100), handshake.completed.notified())
            .await
            .map_err(std::io::Error::other)?;
        drop(first);
        tokio::task::yield_now().await;
        result.map(|_| response.len())
    }

    fn stop(&self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

fn assert_closed(result: &std::io::Result<usize>, path: &str) {
    match result {
        Ok(bytes) => assert_eq!(*bytes, 0, "{path} must not return data"),
        Err(error) => assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
            ),
            "{path} must close or reset the stream, got {error}"
        ),
    }
}

fn bench_proxy_paths(c: &mut Criterion) {
    static SMALL: &[u8] = b"GET / HTTP/1.1\r\nHost: benchmark\r\n\r\n";
    static BULK: [u8; 16 * 1024] = [0x5a; 16 * 1024];

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mux = runtime.block_on(ProxyHarness::start(true, true));
    let legacy = runtime.block_on(ProxyHarness::start(false, true));
    let denied = runtime.block_on(ProxyHarness::start(true, false));
    let halfclose_mux = runtime.block_on(ProxyHarness::start_with_backend(
        true,
        true,
        BackendMode::ReplyAfterEof,
    ));
    let halfclose_legacy = runtime.block_on(ProxyHarness::start_with_backend(
        false,
        true,
        BackendMode::ReplyAfterEof,
    ));
    let reset_mux = runtime.block_on(ProxyHarness::start_with_backend(
        true,
        true,
        BackendMode::Reset,
    ));
    let reset_legacy = runtime.block_on(ProxyHarness::start_with_backend(
        false,
        true,
        BackendMode::Reset,
    ));
    let unused_upstream = SocketAddr::from(([127, 0, 0, 1], reserve_port())).to_string();
    let handshake_failure = runtime.block_on(OutboundHarness::start(
        Some(unused_upstream.clone()),
        1024,
        Arc::new(FailingHandshake),
    ));
    let no_upstream = runtime.block_on(OutboundHarness::start(
        None,
        1024,
        Arc::new(FailingHandshake),
    ));
    let blocking_handshake = Arc::new(BlockingHandshake::new());
    let saturation = runtime.block_on(OutboundHarness::start(
        Some(unused_upstream),
        1,
        blocking_handshake.clone(),
    ));
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_millis(100),
            blocking_handshake.entered.notified(),
        )
        .await
        .unwrap();
        blocking_handshake.release_one();
        tokio::time::timeout(
            Duration::from_millis(100),
            blocking_handshake.completed.notified(),
        )
        .await
        .unwrap();
        tokio::task::yield_now().await;
    });

    let warm = runtime.block_on(mux.request(SMALL)).unwrap();
    assert_eq!(warm, SMALL, "mux benchmark fixture must relay bytes");

    let mut group = c.benchmark_group("proxy_relay");
    group.bench_function("mux_small", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = mux.request(black_box(SMALL)).await.unwrap();
            assert_eq!(response, SMALL);
            black_box(response)
        })
    });
    group.bench_function("legacy_small", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = legacy.request(black_box(SMALL)).await.unwrap();
            assert_eq!(response, SMALL);
            black_box(response)
        })
    });
    group.bench_function("mux_16kb", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = mux.request(black_box(&BULK)).await.unwrap();
            assert_eq!(response.as_slice(), BULK);
            black_box(response)
        })
    });
    group.bench_function("legacy_16kb", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = legacy.request(black_box(&BULK)).await.unwrap();
            assert_eq!(response.as_slice(), BULK);
            black_box(response)
        })
    });
    group.bench_function("policy_denied", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = denied.denied().await;
            assert_closed(&result, "policy denial");
            black_box(result)
        })
    });
    group.bench_function("mux_half_close", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = halfclose_mux.half_close().await.unwrap();
            assert_eq!(response, b"half-close-ok");
            black_box(response)
        })
    });
    group.bench_function("legacy_half_close", |b| {
        b.to_async(&runtime).iter(|| async {
            let response = halfclose_legacy.half_close().await.unwrap();
            assert_eq!(response, b"half-close-ok");
            black_box(response)
        })
    });
    group.bench_function("mux_copy_error", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = reset_mux.denied().await;
            assert_closed(&result, "mux reset");
            black_box(result)
        })
    });
    group.bench_function("legacy_copy_error", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = reset_legacy.denied().await;
            assert_closed(&result, "legacy reset");
            black_box(result)
        })
    });
    group.bench_function("handshake_failure", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = handshake_failure.closed().await;
            assert_closed(&result, "handshake failure");
            black_box(result)
        })
    });
    group.bench_function("no_upstream", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = no_upstream.closed().await;
            assert_closed(&result, "missing upstream");
            black_box(result)
        })
    });
    group.bench_function("saturation_rejection", |b| {
        b.to_async(&runtime).iter(|| async {
            let result = saturation.saturate(&blocking_handshake).await;
            assert_closed(&result, "saturation rejection");
            black_box(result)
        })
    });
    group.finish();

    mux.stop();
    halfclose_mux.stop();
    halfclose_legacy.stop();
    reset_mux.stop();
    reset_legacy.stop();
    legacy.stop();
    denied.stop();
    handshake_failure.stop();
    no_upstream.stop();
    saturation.stop();
}

criterion_group!(
    name = benches;
    config = support::criterion();
    targets = bench_proxy_paths
);
criterion_main!(benches);
