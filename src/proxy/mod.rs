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
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

#[derive(Clone, Debug)]
pub(crate) enum UpstreamTarget {
    Socket(SocketAddr),
    Host { name: Arc<str>, port: Option<u16> },
}

impl UpstreamTarget {
    pub(crate) fn from_config(value: Option<String>) -> Option<Self> {
        let value = value.filter(|value| !value.is_empty())?;
        if let Ok(addr) = value.parse() {
            return Some(Self::Socket(addr));
        }

        let (name, port) = match value.rsplit_once(':') {
            Some((name, port)) => match port.parse::<u16>() {
                Ok(port) => (name, Some(port)),
                Err(_) => (value.as_str(), None),
            },
            None => (value.as_str(), None),
        };
        Some(Self::Host {
            name: Arc::from(name),
            port,
        })
    }
}

impl fmt::Display for UpstreamTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(addr) => addr.fmt(f),
            Self::Host {
                name,
                port: Some(port),
            } => write!(f, "{name}:{port}"),
            Self::Host { name, port: None } => name.fmt(f),
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
mod tests {
    use super::*;

    #[test]
    fn relay_metrics_distinguish_success_from_failure() {
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
