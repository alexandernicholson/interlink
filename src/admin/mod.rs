use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::common::{constants::ports, error::InterlinkError};

/// Administrative HTTP server for health, readiness, and TLS credential reload.
///
/// Endpoints:
/// - `GET /healthz` — liveness probe (always 200 when server is running)
/// - `GET /readyz` — readiness probe (200 when proxies are accepting traffic)
/// - `POST /reload` — atomically reload TLS credentials for new handshakes
#[async_trait::async_trait]
pub trait ReloadHandler: Send + Sync {
    async fn reload(&self) -> Result<(), InterlinkError>;
}

pub struct AdminServer {
    port: u16,
    ready: Arc<std::sync::atomic::AtomicBool>,
    shutdown: Option<watch::Receiver<bool>>,
    reload_handler: Option<Arc<dyn ReloadHandler>>,
}

const HEALTH_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
const READY_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nready";
const NOT_READY_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot ready";
const RELOAD_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 15\r\nConnection: close\r\n\r\nreload complete";
const RELOAD_FAILED_RESPONSE: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 13\r\nConnection: close\r\n\r\nreload failed";
const RELOAD_UNAVAILABLE_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 18\r\nConnection: close\r\n\r\nreload unavailable";
const NOT_FOUND_RESPONSE: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found";

impl AdminServer {
    pub fn new() -> Self {
        Self {
            port: ports::ADMIN,
            ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            shutdown: None,
            reload_handler: None,
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

    pub fn with_reload_handler(mut self, reload_handler: Arc<dyn ReloadHandler>) -> Self {
        self.reload_handler = Some(reload_handler);
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
            (b"POST", b"/reload") => match &self.reload_handler {
                Some(handler) => match handler.reload().await {
                    Ok(()) => {
                        info!("TLS credentials reloaded at request of {}", peer_addr);
                        RELOAD_RESPONSE
                    }
                    Err(error) => {
                        error!(%error, %peer_addr, "TLS credential reload failed");
                        RELOAD_FAILED_RESPONSE
                    }
                },
                None => {
                    warn!(%peer_addr, "TLS credential reload is unavailable");
                    RELOAD_UNAVAILABLE_RESPONSE
                }
            },
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

    struct FixedReloadHandler {
        fail: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ReloadHandler for FixedReloadHandler {
        async fn reload(&self) -> Result<(), InterlinkError> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.fail {
                Err(InterlinkError::Config("invalid replacement".into()))
            } else {
                Ok(())
            }
        }
    }

    async fn request(admin: &AdminServer, request: &'static [u8]) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(request).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            response
        });
        let (stream, peer_address) = listener.accept().await.unwrap();
        admin.handle_request(stream, peer_address).await;
        client.await.unwrap()
    }

    #[tokio::test]
    async fn reload_is_unavailable_without_a_handler() {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            request(&AdminServer::new(), b"POST /reload HTTP/1.1\r\n\r\n"),
        )
        .await
        .expect("admin request timed out");

        assert!(response.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n"));
    }

    #[tokio::test]
    async fn reload_reports_handler_success() {
        let handler = Arc::new(FixedReloadHandler {
            fail: false,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let admin = AdminServer::new().with_reload_handler(handler.clone());
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            request(&admin, b"POST /reload HTTP/1.1\r\n\r\n"),
        )
        .await
        .expect("admin request timed out");

        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(handler.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn reload_reports_handler_failure() {
        let handler = Arc::new(FixedReloadHandler {
            fail: true,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let admin = AdminServer::new().with_reload_handler(handler.clone());
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            request(&admin, b"POST /reload HTTP/1.1\r\n\r\n"),
        )
        .await
        .expect("admin request timed out");

        assert!(response.starts_with(b"HTTP/1.1 500 Internal Server Error\r\n"));
        assert_eq!(handler.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
