//! Frontend example service — connects to the backend via interlink mTLS.
//!
//! Run after `ca_bootstrap` and `backend`:
//!   cargo run --example frontend -- /tmp/interlink-demo 127.0.0.1:8443

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use interlink::common::error::InterlinkError;
use interlink::common::identity::{IdentityProvider, SpiffeId, TrustDomain};
use interlink::proxy::handshake::{TlsClient, TlsHandshake};
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
                .add_directive("frontend=debug".parse().unwrap()),
        )
        .init();

    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("interlink-demo"));

    let backend_addr = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "127.0.0.1:8443".to_string());

    info!("Frontend loading certs from: {:?}", dir);
    let ca_der = std::fs::read(dir.join("ca.der")).expect("read ca.der");
    let client_cert = std::fs::read(dir.join("client.der")).expect("read client.der");
    let client_key = std::fs::read(dir.join("client.key")).expect("read client.key");

    let client_id =
        SpiffeId::try_new("example.local", "default", "frontend").expect("valid SPIFFE ID");
    let td = TrustDomain::new("example.local").with_ca(ca_der);

    let provider = Arc::new(FileIdentityProvider {
        identity: client_id,
        trust_domain: td,
    });

    let tls_client = TlsClient::with_client_auth(
        provider,
        CertificateDer::from(client_cert),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client_key)),
    )
    .expect("TlsClient init");

    // Build the connection address. We connect to the IP but tell TLS we want "localhost"
    // so the certificate's DNS name matches.
    let (_host, port) = backend_addr
        .split_once(':')
        .unwrap_or(("localhost", "8443"));
    let connect_addr = format!("localhost:{}", port);

    info!(
        "Connecting to backend at {} (TLS SNI: localhost) via mTLS...",
        backend_addr
    );
    match tls_client.connect(&connect_addr).await {
        Ok(mut tls_stream) => {
            let peer_id = &tls_stream.peer_identity;
            info!("mTLS connected! Backend identity = {}", peer_id);

            // Send a message
            let msg = "Hello from frontend!";
            tls_stream
                .inner
                .write_all(msg.as_bytes())
                .await
                .expect("write");
            tls_stream.inner.flush().await.ok();
            info!("Sent: {}", msg);

            // Read response
            let mut buf = [0u8; 4096];
            match timeout(Duration::from_secs(5), tls_stream.inner.read(&mut buf)).await {
                Ok(Ok(n)) => {
                    let response = String::from_utf8_lossy(&buf[..n]);
                    info!("Received: {}", response);
                    println!("✓ mTLS echo test PASSED");
                    println!("  Request:  {}", msg);
                    println!("  Response: {}", response);
                }
                Ok(Err(e)) => warn!("Read error: {}", e),
                Err(_) => warn!("Response timeout"),
            }
        }
        Err(e) => {
            warn!("mTLS connection failed: {}", e);
            println!("✗ mTLS connection FAILED: {}", e);
        }
    }
}
