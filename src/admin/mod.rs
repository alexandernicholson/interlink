use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tracing::{error, info, warn};

use crate::common::{
    constants::{admin, ports},
    error::InterlinkError,
};

/// Handler for an authenticated administrative TLS credential reload.
#[async_trait::async_trait]
pub trait ReloadHandler: Send + Sync {
    async fn reload(&self) -> Result<(), InterlinkError>;
}

/// Bounded HTTP health/readiness server. Reloads require a bearer token unless
/// the listener is loopback-only; remote listeners without a token fail closed.
pub struct AdminServer {
    bind: SocketAddr,
    token: Option<String>,
    ready: std::sync::atomic::AtomicBool,
    shutdown: Option<watch::Receiver<bool>>,
    reload_handler: Option<Arc<dyn ReloadHandler>>,
    connections: Arc<Semaphore>,
}

const HEALTH_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
const READY_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nConnection: close\r\n\r\nready";
const NOT_READY_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot ready";
const RELOAD_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 15\r\nConnection: close\r\n\r\nreload complete";
const RELOAD_FAILED_RESPONSE: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: 13\r\nConnection: close\r\n\r\nreload failed";
const RELOAD_UNAVAILABLE_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 18\r\nConnection: close\r\n\r\nreload unavailable";
const UNAUTHORIZED_RESPONSE: &[u8] = b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const NOT_FOUND_RESPONSE: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found";

impl AdminServer {
    pub fn new() -> Self {
        Self {
            bind: ([127, 0, 0, 1], ports::ADMIN).into(),
            token: None,
            ready: std::sync::atomic::AtomicBool::new(false),
            shutdown: None,
            reload_handler: None,
            // One permit per active handler, returned on every exit (including
            // timeout). Excess accepted sockets are dropped, never queued as tasks.
            connections: Arc::new(Semaphore::new(admin::MAX_CONNECTIONS)),
        }
    }

    pub fn with_bind(mut self, bind: SocketAddr) -> Self {
        self.bind = bind;
        self
    }

    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
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

    /// Mark readiness after proxy startup and clear it before shutdown.
    pub fn set_ready(&self, ready: bool) {
        self.ready
            .store(ready, std::sync::atomic::Ordering::Release);
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run().await })
    }

    pub async fn run(self: Arc<Self>) {
        let listener = match TcpListener::bind(self.bind).await {
            Ok(listener) => listener,
            Err(error) => {
                error!(%error, bind = %self.bind, "failed to bind admin server");
                return;
            }
        };
        info!(bind = %self.bind, "admin server listening");
        if self.token.is_none() && !self.bind.ip().is_loopback() {
            warn!(bind = %self.bind, "remote admin reload disabled: INTERLINK_ADMIN_TOKEN is unset");
        }
        self.run_listener(listener).await;
    }

    async fn run_listener(self: Arc<Self>, listener: TcpListener) {
        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (stream, peer_addr) = match accept_result {
                        Ok(connection) => connection,
                        Err(error) => {
                            error!(%error, "admin accept error");
                            continue;
                        }
                    };
                    let permit = match self.connections.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => continue,
                    };
                    let this = self.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
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
                let _ = rx.wait_for(|shutdown| *shutdown).await;
            }
            None => std::future::pending().await,
        }
    }

    fn reload_authorization(&self, headers: &[u8]) -> Option<&'static [u8]> {
        let Some(token) = &self.token else {
            return if self.bind.ip().is_loopback() {
                None
            } else {
                warn!("admin reload refused: non-loopback bind without INTERLINK_ADMIN_TOKEN");
                Some(RELOAD_UNAVAILABLE_RESPONSE)
            };
        };
        let mut authorization = None;
        for line in headers.split(|byte| *byte == b'\n').skip(1) {
            let Some(separator) = line.iter().position(|byte| *byte == b':') else {
                continue;
            };
            let (name, rest) = line.split_at(separator);
            let Some(value) = rest.strip_prefix(b":") else {
                continue;
            };
            if name.eq_ignore_ascii_case(b"authorization") {
                if authorization.is_some() {
                    return Some(UNAUTHORIZED_RESPONSE);
                }
                authorization = Some(value.trim_ascii());
            }
        }
        let supplied = authorization.and_then(|value| value.strip_prefix(b"Bearer "));
        // No non-deprecated ring entry point exposes the contract-required API:
        // verify_slices_are_equal itself is deprecated, so this call cannot avoid it.
        #[allow(deprecated)]
        let authenticated = supplied.is_some_and(|value| {
            !token.is_empty()
                && ring::constant_time::verify_slices_are_equal(value, token.as_bytes()).is_ok()
        });
        if authenticated {
            None
        } else {
            Some(UNAUTHORIZED_RESPONSE)
        }
    }

    async fn handle_request(&self, mut stream: TcpStream, peer_addr: SocketAddr) {
        let mut buf = [0u8; 1024];
        // Bound the entire header read, not each fragment, so trickling bytes
        // cannot indefinitely occupy a connection permit.
        let headers = tokio::time::timeout(admin::READ_TIMEOUT, async {
            let mut used = 0;
            loop {
                let read = stream.read(buf.get_mut(used..)?).await.ok()?;
                if read == 0 {
                    return None;
                }
                used += read;
                if let Some(end) = buf.get(..used)?.windows(4).position(|w| w == b"\r\n\r\n") {
                    return Some(end + 4);
                }
                if used == buf.len() {
                    return None;
                }
            }
        })
        .await;
        let Ok(Some(length)) = headers else {
            return;
        };
        let Some(headers) = buf.get(..length) else {
            return;
        };
        let first_line = headers
            .split(|byte| *byte == b'\n')
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
                if let Some(response) = self.reload_authorization(headers) {
                    response
                } else {
                    match &self.reload_handler {
                        Some(handler) => match handler.reload().await {
                            Ok(()) => {
                                info!(%peer_addr, "TLS credentials reloaded");
                                RELOAD_RESPONSE
                            }
                            Err(error) => {
                                error!(%error, %peer_addr, "TLS credential reload failed");
                                RELOAD_FAILED_RESPONSE
                            }
                        },
                        None => RELOAD_UNAVAILABLE_RESPONSE,
                    }
                }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct FixedReloadHandler {
        fail: bool,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ReloadHandler for FixedReloadHandler {
        async fn reload(&self) -> Result<(), InterlinkError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                Err(InterlinkError::Config("invalid replacement".into()))
            } else {
                Ok(())
            }
        }
    }

    fn handler(fail: bool) -> Arc<FixedReloadHandler> {
        Arc::new(FixedReloadHandler {
            fail,
            calls: AtomicUsize::new(0),
        })
    }

    async fn request(admin: &AdminServer, request: &'static [u8]) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(1), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let client = tokio::spawn(async move {
                let mut stream = TcpStream::connect(address).await.unwrap();
                stream.write_all(request).await.unwrap();
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await.unwrap();
                response
            });
            let (stream, peer) = listener.accept().await.unwrap();
            admin.handle_request(stream, peer).await;
            client.await.unwrap()
        })
        .await
        .expect("admin request timed out")
    }

    #[tokio::test]
    async fn admin_health_and_readiness_lifecycle() {
        let admin = AdminServer::new();
        assert!(request(&admin, b"GET /healthz HTTP/1.1\r\n\r\n")
            .await
            .starts_with(b"HTTP/1.1 200"));
        assert!(request(&admin, b"GET /readyz HTTP/1.1\r\n\r\n")
            .await
            .starts_with(b"HTTP/1.1 503"));
        admin.set_ready(true);
        assert!(request(&admin, b"GET /readyz HTTP/1.1\r\n\r\n")
            .await
            .starts_with(b"HTTP/1.1 200"));
        admin.set_ready(false);
        assert!(request(&admin, b"GET /readyz HTTP/1.1\r\n\r\n")
            .await
            .starts_with(b"HTTP/1.1 503"));
        assert!(request(&admin, b"GET /missing HTTP/1.1\r\n\r\n")
            .await
            .starts_with(b"HTTP/1.1 404"));
    }

    #[tokio::test]
    async fn reload_without_token_allowed_only_on_loopback() {
        let handler = handler(false);
        let admin = AdminServer::new().with_reload_handler(handler.clone());
        assert!(request(&admin, b"POST /reload HTTP/1.1\r\n\r\n")
            .await
            .starts_with(b"HTTP/1.1 200"));
        assert_eq!(handler.calls.load(Ordering::Relaxed), 1);
        let admin = admin.with_bind(([0, 0, 0, 0], 4192).into());
        assert!(request(&admin, b"POST /reload HTTP/1.1\r\n\r\n")
            .await
            .starts_with(b"HTTP/1.1 503"));
        assert_eq!(handler.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn reload_checks_bearer_token_before_calling_handler() {
        let handler = handler(false);
        let admin = AdminServer::new()
            .with_bind(([0, 0, 0, 0], 4192).into())
            .with_token(Some("secret".into()))
            .with_reload_handler(handler.clone());
        for request_bytes in [
            b"POST /reload HTTP/1.1\r\n\r\n".as_slice(),
            b"POST /reload HTTP/1.1\r\nAuthorization: Bearer wrong\r\n\r\n".as_slice(),
            b"POST /reload HTTP/1.1\r\nAuthorization: Bearer secret\r\nAuthorization: Bearer secret\r\n\r\n".as_slice(),
        ] {
            assert!(request(&admin, request_bytes).await.starts_with(b"HTTP/1.1 401"));
            assert_eq!(handler.calls.load(Ordering::Relaxed), 0);
        }
        assert!(request(
            &admin,
            b"POST /reload HTTP/1.1\r\nauthorization: Bearer secret\r\n\r\n"
        )
        .await
        .starts_with(b"HTTP/1.1 200"));
        assert_eq!(handler.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn reload_unavailable_and_handler_failure() {
        assert!(
            request(&AdminServer::new(), b"POST /reload HTTP/1.1\r\n\r\n")
                .await
                .starts_with(b"HTTP/1.1 503")
        );
        let handler = handler(true);
        let admin = AdminServer::new().with_reload_handler(handler.clone());
        assert!(request(&admin, b"POST /reload HTTP/1.1\r\n\r\n")
            .await
            .starts_with(b"HTTP/1.1 500"));
        assert_eq!(handler.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn admin_reads_fragmented_authorization_headers() {
        tokio::time::timeout(Duration::from_secs(1), async {
            let handler = handler(false);
            let admin = Arc::new(
                AdminServer::new()
                    .with_token(Some("secret".into()))
                    .with_reload_handler(handler.clone()),
            );
            let (address, server) = start_listener(admin).await;
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream
                .write_all(b"POST /reload HTTP/1.1\r\nAuthor")
                .await
                .unwrap();
            tokio::task::yield_now().await;
            assert_eq!(handler.calls.load(Ordering::Relaxed), 0);
            stream
                .write_all(b"ization: Bearer secret\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200"));
            assert_eq!(handler.calls.load(Ordering::Relaxed), 1);
            server.abort();
        })
        .await
        .expect("fragmented request timed out");
    }

    #[tokio::test]
    async fn admin_rejects_oversized_headers_without_reload() {
        tokio::time::timeout(Duration::from_secs(1), async {
            let handler = handler(false);
            let admin = Arc::new(AdminServer::new().with_reload_handler(handler.clone()));
            let (address, server) = start_listener(admin).await;
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(&[b'x'; 1024]).await.unwrap();
            assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            assert_eq!(handler.calls.load(Ordering::Relaxed), 0);
            server.abort();
        })
        .await
        .expect("oversized request timed out");
    }

    #[tokio::test]
    async fn admin_configured_bind_and_shutdown() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = reservation.local_addr().unwrap();
            drop(reservation);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let admin = Arc::new(
                AdminServer::new()
                    .with_bind(address)
                    .with_shutdown(shutdown_rx),
            );
            let server = admin.spawn();
            let mut connection = loop {
                match TcpStream::connect(address).await {
                    Ok(connection) => break connection,
                    Err(_) => tokio::task::yield_now().await,
                }
            };
            assert_eq!(connection.peer_addr().unwrap(), address);
            connection
                .write_all(b"GET /healthz HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            connection.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200"));
            shutdown_tx.send(true).unwrap();
            server.await.unwrap();
            assert!(TcpStream::connect(address).await.is_err());
        })
        .await
        .expect("admin bind/shutdown timed out");
    }

    async fn start_listener(admin: Arc<AdminServer>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        (address, tokio::spawn(admin.run_listener(listener)))
    }

    #[tokio::test]
    async fn admin_idle_connection_dropped_within_read_timeout() {
        let (address, server) = start_listener(Arc::new(AdminServer::new())).await;
        let mut stream = TcpStream::connect(address).await.unwrap();
        let result = tokio::time::timeout(
            admin::READ_TIMEOUT + Duration::from_secs(1),
            stream.read(&mut [0]),
        )
        .await
        .expect("idle connection not dropped")
        .unwrap();
        assert_eq!(result, 0);
        server.abort();
    }

    #[tokio::test]
    async fn admin_connection_cap_and_permit_release() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let admin = Arc::new(AdminServer::new());
            let (address, server) = start_listener(admin.clone()).await;
            let mut clients = Vec::new();
            for _ in 0..admin::MAX_CONNECTIONS {
                clients.push(TcpStream::connect(address).await.unwrap());
            }
            while admin.connections.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
            let mut excess = TcpStream::connect(address).await.unwrap();
            assert_eq!(excess.read(&mut [0]).await.unwrap(), 0);
            let mut client = clients.pop().unwrap();
            client
                .write_all(b"GET /healthz HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200"));
            while admin.connections.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
            let mut replacement = TcpStream::connect(address).await.unwrap();
            replacement
                .write_all(b"GET /healthz HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            response.clear();
            replacement.read_to_end(&mut response).await.unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200"));
            drop(clients);
            server.abort();
        })
        .await
        .expect("connection-cap test timed out");
    }
}
