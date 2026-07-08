//! Benchmark server — HTTPS echo with optional fixed delay.
//!
//! Usage:
//!   cargo run --example ca_bootstrap
//!   cargo run --example bench_server -- /tmp/interlink-demo 8443 200ms

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use interlink::common::error::InterlinkError;
use interlink::common::identity::{IdentityProvider, SpiffeId, TrustDomain};
use interlink::proxy::handshake::{TlsHandshake, TlsServer};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

struct FileIdentityProvider {
    identity: SpiffeId,
    trust_domain: TrustDomain,
}

impl IdentityProvider for FileIdentityProvider {
    fn get_identity(&self) -> Result<SpiffeId, InterlinkError> {
        Ok(self.identity.clone())
    }
    fn get_trust_domain(&self) -> &TrustDomain {
        &self.trust_domain
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("interlink=info".parse().unwrap())
                .add_directive("bench_server=info".parse().unwrap()),
        )
        .init();

    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("interlink-demo"));

    let port: u16 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8443);

    let delay_ms: u64 = std::env::args()
        .nth(3)
        .and_then(|s| s.trim_end_matches("ms").parse().ok())
        .unwrap_or(0);
    let delay = Duration::from_millis(delay_ms);

    info!("Bench server loading certs from: {:?}", dir);
    let ca_der = std::fs::read(dir.join("ca.der")).expect("read ca.der");
    let server_cert = std::fs::read(dir.join("server.der")).expect("read server.der");
    let server_key = std::fs::read(dir.join("server.key")).expect("read server.key");

    let server_id =
        SpiffeId::try_new("example.local", "default", "backend").expect("valid SPIFFE ID");
    let td = TrustDomain::new("example.local").with_ca(ca_der);

    let provider = Arc::new(FileIdentityProvider {
        identity: server_id,
        trust_domain: td,
    });

    let tls_server = Arc::new(
        TlsServer::new(
            provider,
            CertificateDer::from(server_cert),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key)),
        )
        .expect("TlsServer init"),
    );

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .expect("bind listener");
    info!("Bench server listening on port {} delay {:?}", port, delay);

    loop {
        let (stream, _peer) = listener.accept().await.expect("accept");

        let tls = tls_server.clone();
        tokio::spawn(async move {
            match tls.accept(stream).await {
                Ok(mut tls_stream) => {
                    let peer_id = &tls_stream.peer_identity;
                    info!("mTLS handshake complete, peer identity = {}", peer_id);

                    let mut buf = [0u8; 8192];
                    loop {
                        match tls_stream.inner.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                let (status, body) = handle_http_request(&buf[..n], delay).await;
                                let response = format_http_response(status, &body);
                                if let Err(e) = tls_stream.inner.write_all(&response).await {
                                    warn!("Write error: {}", e);
                                    break;
                                }
                                let _ = tls_stream.inner.flush().await;
                            }
                            Err(e) => {
                                warn!("Read error: {}", e);
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("mTLS handshake failed: {}", e);
                }
            }
        });
    }
}

async fn handle_http_request(req: &[u8], delay: Duration) -> (u16, Vec<u8>) {
    if delay > Duration::ZERO {
        tokio::time::sleep(delay).await;
    }

    // Parse the request line from the first line of headers.
    let header_end = req
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(req.len());
    let headers = &req[..header_end];
    let header_text = String::from_utf8_lossy(headers);
    let mut lines = header_text.lines();
    let first = lines.next().unwrap_or("");
    let parts: Vec<&str> = first.split_whitespace().collect();
    if parts.len() != 3 || !parts[2].starts_with("HTTP/1.") {
        return (400, b"bad request".to_vec());
    }

    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    let body = if content_length > 0 && header_end + content_length <= req.len() {
        req[header_end..header_end + content_length].to_vec()
    } else {
        Vec::new()
    };

    (200, body)
}

fn format_http_response(status: u16, body: &[u8]) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Unknown",
    };
    let mut response = Vec::new();
    write!(
        &mut response,
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: keep-alive\r\n\r\n",
        status, status_text, body.len()
    )
    .unwrap();
    response.extend_from_slice(body);
    response
}
