// B8: No panic paths in connection-handling code.
// Test code is exempt via cfg(test).
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub mod handshake;
pub(crate) mod original_dst;
pub mod outbound;
pub mod tcp;

pub use handshake::{TlsClient, TlsHandshake, TlsServer};
pub(crate) use original_dst::get_original_dst;
pub use outbound::OutboundProxy;
pub use tcp::TcpProxy;

use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// Buffer size for bidirectional data copy (8 KiB, matching tokio default).
/// 64 KiB was tested but caused heavy-profile p99 regression (84 vs 44 ms)
/// at 6400 connections, likely from cache pressure. Revisit with a proper
/// benchmark-driven tuning pass.
pub(crate) const COPY_BUF_SIZE: usize = 8192;

/// Bidirectional copy with 64 KiB buffers.
///
/// Delegates to tokio's `copy_bidirectional_with_sizes`, which propagates
/// half-close: when one side reaches EOF, the other side is shut down
/// (FIN / TLS close_notify), per C7 — protocols that read-to-EOF depend on it.
pub(crate) async fn copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    tokio::io::copy_bidirectional_with_sizes(a, b, COPY_BUF_SIZE, COPY_BUF_SIZE).await
}

/// Bind a SO_REUSEPORT listener for one acceptor task.
///
/// Fallible by design (B15): bind and listen fail in ordinary circumstances
/// (port taken by a non-reuseport socket, permission denied on privileged
/// ports) — callers log and count failures; they must not panic (B8).
pub(crate) fn bind_reuseport(addr: std::net::SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
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
pub(crate) fn num_acceptors() -> usize {
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
