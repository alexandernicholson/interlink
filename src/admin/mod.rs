use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::common::constants::ports;

/// Administrative HTTP server for health, readiness, and config reload.
///
/// Endpoints:
/// - `GET /healthz` — liveness probe (always 200 when server is running)
/// - `GET /readyz` — readiness probe (200 when proxies are accepting traffic)
/// - `POST /reload` — trigger configuration reload (placeholder)
pub struct AdminServer {
    port: u16,
    ready: Arc<std::sync::atomic::AtomicBool>,
    shutdown: Option<watch::Receiver<bool>>,
}

const HEALTH_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
const READY_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nready";
const NOT_READY_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot ready";
const RELOAD_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 19\r\nConnection: close\r\n\r\nreload acknowledged";
const NOT_FOUND_RESPONSE: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found";

impl AdminServer {
    pub fn new() -> Self {
        Self {
            port: ports::ADMIN,
            ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            shutdown: None,
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_shutdown(mut self, shutdown: watch::Receiver<bool>) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    /// Mark the service as ready (or not ready) for traffic.
    pub fn set_ready(&self, ready: bool) {
        self.ready
            .store(ready, std::sync::atomic::Ordering::Release);
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    pub async fn run(self: Arc<Self>) {
        let addr = format!("0.0.0.0:{}", self.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                error!("failed to bind admin server {}: {}", addr, e);
                return;
            }
        };
        info!("admin server listening on {}", addr);

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (stream, peer_addr) = match accept_result {
                        Ok(s) => s,
                        Err(e) => {
                            error!("admin accept error: {}", e);
                            continue;
                        }
                    };
                    let this = self.clone();
                    tokio::spawn(async move {
                        this.handle_request(stream, peer_addr).await;
                    });
                }
                _ = Self::wait_shutdown(self.shutdown.as_ref()) => {
                    info!("admin server received shutdown signal");
                    break;
                }
            }
        }
    }

    async fn wait_shutdown(shutdown: Option<&watch::Receiver<bool>>) {
        match shutdown {
            Some(rx) => {
                let mut rx = rx.clone();
                let _ = rx.changed().await;
            }
            None => std::future::pending().await,
        }
    }

    async fn handle_request(&self, mut stream: TcpStream, peer_addr: std::net::SocketAddr) {
        let mut buf = [0u8; 1024];
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };

        let first_line = buf[..n]
            .split(|&byte| byte == b'\n')
            .next()
            .unwrap_or_default();
        let mut parts = first_line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|part| !part.is_empty());
        let method = parts.next().unwrap_or_default();
        let path = parts.next().unwrap_or_default();
        let response = match (method, path) {
            (b"GET", b"/healthz") => HEALTH_RESPONSE,
            (b"GET", b"/readyz") => {
                if self.ready.load(std::sync::atomic::Ordering::Acquire) {
                    READY_RESPONSE
                } else {
                    NOT_READY_RESPONSE
                }
            }
            (b"POST", b"/reload") => {
                warn!(
                    "config reload requested from {} (not yet implemented)",
                    peer_addr
                );
                RELOAD_RESPONSE
            }
            _ => NOT_FOUND_RESPONSE,
        };

        let _ = stream.write_all(response).await;
        let _ = stream.shutdown().await;
    }
}

impl Default for AdminServer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_admin_healthz() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let admin = Arc::new(AdminServer::new().with_port(port));
        let handle = admin.spawn();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf[..n]);
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("ok"));

        handle.abort();
    }

    #[tokio::test]
    async fn test_admin_readyz_not_ready() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let admin = Arc::new(AdminServer::new().with_port(port));
        admin.set_ready(false);
        let handle = admin.spawn();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /readyz HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf[..n]);
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));

        handle.abort();
    }
}
