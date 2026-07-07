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
use tokio::net::TcpStream;

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
