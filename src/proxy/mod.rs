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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Buffer size for bidirectional data copy (64 KiB).
/// Used by the custom `copy_bidirectional` in place of tokio's hardcoded 8 KiB.
pub(crate) const COPY_BUF_SIZE: usize = 65536;

/// Copy data bidirectionally between two async streams using 64 KiB buffers.
///
/// Uses a `select!` loop to read from whichever side is ready first, then
/// writes to the other. The buffer is `COPY_BUF_SIZE` bytes per direction.
pub(crate) async fn copy_bidirectional<A, B>(
    a: &mut A,
    b: &mut B,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf_a = vec![0u8; COPY_BUF_SIZE];
    let mut buf_b = vec![0u8; COPY_BUF_SIZE];
    let mut read_a = false;
    let mut read_b = false;
    let mut done_a = false;
    let mut done_b = false;
    let mut total_a = 0u64;
    let mut total_b = 0u64;

    loop {
        tokio::select! {
            biased;
            // Read from A if not in progress and not done.
            result = async { if !read_a && !done_a {
                let n = a.read(&mut buf_a).await;
                read_a = true;
                n
            } else { std::future::pending::<std::io::Result<usize>>().await } } => {
                match result {
                    Ok(0) => { done_a = true; read_a = false; }
                    Ok(n) => { total_a += n as u64; b.write_all(&buf_a[..n]).await?; read_a = false; }
                    Err(e) => return Err(e),
                }
            }
            // Read from B if not in progress and not done.
            result = async { if !read_b && !done_b {
                let n = b.read(&mut buf_b).await;
                read_b = true;
                n
            } else { std::future::pending::<std::io::Result<usize>>().await } } => {
                match result {
                    Ok(0) => { done_b = true; read_b = false; }
                    Ok(n) => { total_b += n as u64; a.write_all(&buf_b[..n]).await?; read_b = false; }
                    Err(e) => return Err(e),
                }
            }
        }

        if done_a && done_b {
            return Ok((total_a, total_b));
        }

        // Flush writes after each round.
        b.flush().await?;
        a.flush().await?;
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
