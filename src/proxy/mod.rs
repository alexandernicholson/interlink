// B8: No panic paths in connection-handling code.
// Test code is exempt via cfg(test).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub mod handshake;
pub(crate) mod mux;
pub(crate) mod original_dst;
pub mod outbound;
pub mod reload;
pub mod tcp;
pub(crate) mod verify;

pub use handshake::{TlsClient, TlsHandshake, TlsServer};
pub(crate) use original_dst::get_original_dst;
pub use outbound::OutboundProxy;
pub use reload::{TlsCredentialPaths, TlsCredentialReloader};
pub use tcp::TcpProxy;

use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// Injectable upstream I/O (B11); deadlines apply to production TCP dials.
#[async_trait::async_trait]
pub(crate) trait UpstreamDialer: Send + Sync {
    async fn connect(&self, addr: SocketAddr) -> std::io::Result<TcpStream>;
}

struct TcpDialer;

#[async_trait::async_trait]
impl UpstreamDialer for TcpDialer {
    async fn connect(&self, addr: SocketAddr) -> std::io::Result<TcpStream> {
        tcp_connect(TcpStream::connect(addr)).await
    }
}

async fn tcp_connect(
    connect: impl Future<Output = std::io::Result<TcpStream>>,
) -> std::io::Result<TcpStream> {
    tokio::time::timeout(crate::common::constants::timeouts::TCP_CONNECT, connect)
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upstream TCP connect timed out",
            )
        })?
}

pub(crate) trait OriginalDestination: Send + Sync {
    fn get(&self, stream: &TcpStream) -> Option<SocketAddr>;
}

struct NetfilterDestination;

impl OriginalDestination for NetfilterDestination {
    fn get(&self, stream: &TcpStream) -> Option<SocketAddr> {
        get_original_dst(stream)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum UpstreamTarget {
    Socket(SocketAddr),
    /// Keep hostname:port intact so discovery never fabricates a service port.
    Host(Arc<str>),
}

impl UpstreamTarget {
    pub(crate) fn from_config(value: Option<String>) -> Option<Self> {
        let value = value.filter(|value| !value.is_empty())?;
        if let Ok(addr) = value.parse() {
            return Some(Self::Socket(addr));
        }

        Some(Self::Host(Arc::from(value)))
    }
}

impl fmt::Display for UpstreamTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(addr) => addr.fmt(f),
            Self::Host(name) => name.fmt(f),
        }
    }
}

/// Buffer size for bidirectional data copy.
///
/// Default 64 KiB, decided by the R30 interleaved bulk-profile A/B
/// (256 KB payloads: p50 −15 %, proxy CPU −22 % vs 8 KiB; no measurable
/// effect on 1 KB-payload workloads, whose requests never fill either
/// buffer). Overridable via `INTERLINK_COPY_BUF_SIZE` (4 KiB..=1 MiB).
pub(crate) fn copy_buf_size() -> usize {
    static SIZE: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        std::env::var("INTERLINK_COPY_BUF_SIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_COPY_BUF_SIZE)
            .clamp(4096, 1 << 20)
    });
    *SIZE
}

pub(crate) const DEFAULT_COPY_BUF_SIZE: usize = 65536;

/// Bidirectional copy with `copy_buf_size()` buffers.
///
/// Delegates to tokio's `copy_bidirectional_with_sizes`, which propagates
/// half-close: when one side reaches EOF, the other side is shut down
/// (FIN / TLS close_notify), per C7 — protocols that read-to-EOF depend on it.
pub(crate) async fn copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let n = copy_buf_size();
    tokio::io::copy_bidirectional_with_sizes(a, b, n, n).await
}

/// Record a relay's metrics without turning failures into zero-valued latency
/// observations. Shared by inbound, outbound, and mux relays (C2/C3).
pub(crate) fn record_relay_result(
    result: std::io::Result<(u64, u64)>,
    start: Instant,
) -> std::io::Result<(u64, u64)> {
    match &result {
        Ok((bytes_up, bytes_down)) => {
            crate::metrics::record_connection(*bytes_up, *bytes_down, start.elapsed());
        }
        Err(_) => crate::metrics::record_connection_failed(),
    }
    result
}

/// Bind a SO_REUSEPORT listener for one acceptor task.
///
/// Fallible by design (B15): bind and listen fail in ordinary circumstances
/// (port taken by a non-reuseport socket, permission denied on privileged
/// ports) — callers log and count failures; they must not panic (B8).
pub(crate) fn bind_reuseport(
    addr: std::net::SocketAddr,
) -> std::io::Result<tokio::net::TcpListener> {
    let domain = if addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let sock = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&socket2::SockAddr::from(addr))?;
    sock.listen(1024)?;
    let std_listener: std::net::TcpListener = sock.into();
    tokio::net::TcpListener::from_std(std_listener)
}

/// Number of SO_REUSEPORT acceptor tasks per proxy listener.
///
/// Overridable via `INTERLINK_ACCEPTORS` (clamped to 1..=16) so deployments —
/// and A/B benchmarks — can tune or disable multi-acceptor without a rebuild.
pub(crate) fn num_acceptors() -> usize {
    if let Some(n) = std::env::var("INTERLINK_ACCEPTORS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        return n.clamp(1, 16);
    }
    std::cmp::min(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2),
        4,
    )
    .max(2)
}

/// Wait for the shutdown signal on a proxy's watch channel.
/// `None` (no channel installed) never resolves — the acceptor runs forever.
pub(crate) async fn wait_shutdown(shutdown: Option<tokio::sync::watch::Receiver<bool>>) {
    match shutdown {
        Some(mut rx) => {
            // Already-signalled channels must resolve immediately.
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    // Sender dropped: treat as shutdown.
                    return;
                }
            }
        }
        None => std::future::pending().await,
    }
}

/// Apply standard TCP tuning to a proxy socket.
///
/// - `TCP_NODELAY` reduces latency for small TLS records.
/// - TCP keepalive helps detect dead peers.
pub(crate) fn configure_socket(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let sock_ref = socket2::SockRef::from(stream);
    sock_ref.set_keepalive(true)?;
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(300));
    sock_ref.set_tcp_keepalive(&keepalive)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::common::error::InterlinkError;
    use crate::common::identity::SpiffeId;
    use crate::proxy::handshake::{TlsHandshake, TlsStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    pub(crate) const WAIT: Duration = Duration::from_secs(3);

    pub(crate) fn config() -> config::ProxyConfig {
        config::ProxyConfig {
            identity: Some("spiffe://test.local/ns/default/sa/proxy".into()),
            max_connections: Some(4),
            ..Default::default()
        }
    }

    pub(crate) struct ExpectedEndpoint;

    #[async_trait::async_trait]
    impl crate::discovery::dns::DnsResolver for ExpectedEndpoint {
        async fn lookup(&self, name: &str) -> Result<Vec<SocketAddr>, InterlinkError> {
            assert_eq!(name, "backend.test:8443");
            Ok(vec!["192.0.2.7:8443".parse().unwrap()])
        }
    }

    pub(crate) struct DirectDestination;

    impl OriginalDestination for DirectDestination {
        fn get(&self, stream: &TcpStream) -> Option<SocketAddr> {
            // Model non-redirected SO_ORIGINAL_DST on every test platform.
            stream.local_addr().ok()
        }
    }

    pub(crate) struct GateDialer {
        pub calls: AtomicUsize,
        pub release: Semaphore,
    }

    impl GateDialer {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                release: Semaphore::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl UpstreamDialer for GateDialer {
        async fn connect(&self, addr: SocketAddr) -> std::io::Result<TcpStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _gate = self.release.acquire().await.unwrap();
            TcpDialer.connect(addr).await
        }
    }

    pub(crate) struct GateHandshake {
        pub calls: AtomicUsize,
        pub release: Semaphore,
        acceptor: Option<tokio_rustls::TlsAcceptor>,
    }

    #[async_trait::async_trait]
    impl TlsHandshake for GateHandshake {
        async fn connect(&self, _addr: SocketAddr) -> Result<TlsStream, InterlinkError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _gate = self.release.acquire().await.unwrap();
            Err(InterlinkError::Protocol(
                "injected handshake failure".into(),
            ))
        }

        async fn accept(&self, stream: TcpStream) -> Result<TlsStream, InterlinkError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _gate = self.release.acquire().await.unwrap();
            let Some(acceptor) = &self.acceptor else {
                return Err(InterlinkError::Protocol(
                    "injected handshake failure".into(),
                ));
            };
            Ok(TlsStream {
                inner: acceptor.accept(stream).await?.into(),
                peer_identity: Arc::new(SpiffeId::from_uri(
                    "spiffe://test.local/ns/default/sa/client",
                )?),
            })
        }
    }

    pub(crate) fn handshake(success: bool) -> (Arc<GateHandshake>, Arc<rustls::ClientConfig>) {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let certified =
            interlink_test_support::rcgen::generate_simple_self_signed(vec!["localhost".into()])
                .unwrap();
        let cert = certified.cert.der().clone();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let client = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert],
                rustls::pki_types::PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der())
                    .into(),
            )
            .unwrap();
        (
            Arc::new(GateHandshake {
                calls: AtomicUsize::new(0),
                release: Semaphore::new(0),
                acceptor: success.then(|| tokio_rustls::TlsAcceptor::from(Arc::new(server))),
            }),
            Arc::new(client),
        )
    }

    pub(crate) async fn pair() -> (TcpStream, TcpStream, SocketAddr) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpDialer
            .connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, peer) = listener.accept().await.unwrap();
        (client, server, peer)
    }

    pub(crate) async fn wait_for(mut ready: impl FnMut() -> bool) {
        tokio::time::timeout(WAIT, async {
            while !ready() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_metrics_distinguish_success_from_failure() {
        // INTERLINK_METRICS binds handles on first use. Other proxy tests can
        // initialize it without a recorder; isolate this recorder assertion.
        const CHILD: &str = "INTERLINK_RELAY_METRICS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proxy::tests::relay_metrics_distinguish_success_from_failure",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::record_connection_start();
            assert_eq!(
                record_relay_result(Ok((7, 11)), Instant::now()).unwrap(),
                (7, 11)
            );

            crate::metrics::record_connection_start();
            let error = record_relay_result(
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected relay reset",
                )),
                Instant::now(),
            )
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        });

        let rendered = handle.render();
        for expected in [
            "interlink_connections_total 2",
            "interlink_connection_errors_total 1",
            "interlink_bytes_total 18",
            "interlink_connection_duration_seconds_count 1",
        ] {
            assert!(
                rendered.lines().any(|line| line == expected),
                "missing `{expected}` in:\n{rendered}"
            );
        }
    }
}

#[cfg(test)]
mod connect_tests {
    use super::*;

    #[tokio::test]
    async fn upstream_tcp_connect_deadline_cancels_pending_dial() {
        struct Cancelled(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Cancelled {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cancelled.clone();
        let pending_dial = async move {
            let _guard = Cancelled(flag);
            std::future::pending().await
        };
        let error = tokio::time::timeout(
            crate::common::constants::timeouts::TCP_CONNECT + test_support::WAIT,
            tcp_connect(pending_dial),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
    }
}
