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

/// Buffer size for bidirectional data copy (64 KiB vs tokio's 8 KiB default).
pub(crate) const COPY_BUF_SIZE: usize = 65536;

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
