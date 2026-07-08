//! Backend example service — listens for mTLS connections using interlink.
//!
//! Run after `ca_bootstrap`:
//!   cargo run --example backend -- /tmp/interlink-demo

use std::path::PathBuf;
use std::sync::Arc;

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
                .add_directive("interlink=debug".parse().unwrap())
                .add_directive("backend=debug".parse().unwrap()),
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

    info!("Backend loading certs from: {:?}", dir);
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
    info!("Backend listening on port {}", port);

    loop {
        let (stream, peer) = listener.accept().await.expect("accept");
        info!("Accepted TCP from {}", peer);

        let tls = tls_server.clone();
        tokio::spawn(async move {
            match tls.accept(stream).await {
                Ok(mut tls_stream) => {
                    let peer_id = &tls_stream.peer_identity;
                    info!("mTLS handshake complete, peer identity = {}", peer_id);

                    // Echo loop
                    let mut buf = [0u8; 4096];
                    loop {
                        match tls_stream.inner.read(&mut buf).await {
                            Ok(0) => {
                                info!("Peer {} closed connection", peer_id);
                                break;
                            }
                            Ok(n) => {
                                info!("Received {} bytes from {}", n, peer_id);
                                let response = format!(
                                    "backend-echo[{}]: {}",
                                    peer_id.service_account,
                                    String::from_utf8_lossy(&buf[..n])
                                );
                                if let Err(e) =
                                    tls_stream.inner.write_all(response.as_bytes()).await
                                {
                                    warn!("Write error: {}", e);
                                    break;
                                }
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
