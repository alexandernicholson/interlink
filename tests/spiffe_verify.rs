//! SPIFFE X.509-SVID server verification (outbound mTLS).
//!
//! The outbound verifier authenticates peers by the SPIFFE URI SAN in their
//! trust domain, not by RFC 6125 name matching — mesh peers are dialed by
//! ephemeral pod/Service IPs that can't appear in a workload cert. These tests
//! pin that it (1) accepts a valid peer dialed by an address NOT in its SAN,
//! yet (2) still enforces full chain validation: a peer from a different CA or
//! a different trust domain is rejected (fail closed).
use std::sync::Arc;

use interlink::common::error::InterlinkError;
use interlink::common::identity::{IdentityProvider, SpiffeId, TrustDomain};
use interlink::identity::ca::CertificateAuthority;
use interlink::proxy::handshake::{TlsClient, TlsServer};
use interlink::proxy::TlsHandshake;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;

struct P {
    id: SpiffeId,
    td: TrustDomain,
}
impl IdentityProvider for P {
    fn get_identity(&self) -> Result<SpiffeId, InterlinkError> {
        Ok(self.id.clone())
    }
    fn get_trust_domain(&self) -> &TrustDomain {
        &self.td
    }
}

/// Start a TlsServer on 127.0.0.1 whose cert is issued by `server_ca` with the
/// given SANs. Returns the port. The accept loop runs one handshake.
async fn spawn_server(server_ca: &CertificateAuthority, td_name: &str, sans: &[&str]) -> u16 {
    let id = SpiffeId::try_new(td_name, "default", "backend").unwrap();
    let (cert, key) = server_ca.issue_leaf_with_key(&id, sans).unwrap();
    let td = TrustDomain::new(td_name).with_ca(server_ca.root_cert_der().to_vec());
    let server = Arc::new(
        TlsServer::new(
            Arc::new(P { id, td }),
            cert,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((s, _)) = listener.accept().await {
            let _ = server.accept(s).await;
        }
    });
    port
}

/// Build a client whose trust domain / CA is `client_ca`.
fn build_client(client_ca: &CertificateAuthority, td_name: &str) -> TlsClient {
    let id = SpiffeId::try_new(td_name, "default", "frontend").unwrap();
    let (cert, key) = client_ca.issue_leaf_with_key(&id, &["localhost"]).unwrap();
    let td = TrustDomain::new(td_name).with_ca(client_ca.root_cert_der().to_vec());
    TlsClient::with_client_auth(
        Arc::new(P { id, td }),
        cert,
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
    )
    .unwrap()
}

/// Positive: a valid peer dialed by an IP that is NOT in its certificate SAN
/// (the cert has only the `localhost` DNS SAN) is accepted, and its SPIFFE
/// identity is extracted. This is the exact case stock WebPKI rejected with
/// `NotValidForName` and that broke meshed ClusterIP traffic.
#[tokio::test]
async fn accepts_peer_dialed_by_address_not_in_san() {
    let ca = CertificateAuthority::new("mesh.local").unwrap();
    let port = spawn_server(&ca, "mesh.local", &["localhost"]).await; // no IP SAN
    let client = build_client(&ca, "mesh.local");

    // Dial by IP literal — not present in the server cert's SANs.
    let tls = client
        .connect(&format!("127.0.0.1:{}", port))
        .await
        .expect("handshake should succeed via SPIFFE identity, not name match");
    assert_eq!(
        tls.peer_identity,
        SpiffeId::try_new("mesh.local", "default", "backend").unwrap()
    );
}

/// Negative: a server whose cert chains to a DIFFERENT CA is rejected — full
/// RFC 5280 path validation is still enforced (fail closed).
#[tokio::test]
async fn rejects_peer_from_untrusted_ca() {
    let server_ca = CertificateAuthority::new("mesh.local").unwrap();
    let other_ca = CertificateAuthority::new("mesh.local").unwrap(); // different root
    let port = spawn_server(&server_ca, "mesh.local", &["localhost"]).await;
    // Client trusts only `other_ca`, not the server's CA.
    let client = build_client(&other_ca, "mesh.local");

    let res = client.connect(&format!("127.0.0.1:{}", port)).await;
    assert!(
        res.is_err(),
        "peer signed by an untrusted CA must be rejected"
    );
}

/// Negative: a valid, well-signed peer in a DIFFERENT trust domain is rejected.
/// (Client and server share a CA but disagree on the trust-domain name.)
#[tokio::test]
async fn rejects_peer_from_wrong_trust_domain() {
    let ca = CertificateAuthority::new("evil.local").unwrap();
    // Server presents a valid cert for evil.local, signed by `ca`.
    let port = spawn_server(&ca, "evil.local", &["localhost"]).await;
    // Client trusts `ca` (so the chain validates) but expects trust domain
    // "mesh.local" — the SPIFFE trust-domain check must reject it.
    let client = build_client(&ca, "mesh.local");

    let res = client.connect(&format!("127.0.0.1:{}", port)).await;
    assert!(
        res.is_err(),
        "peer in a different trust domain must be rejected even with a valid chain"
    );
}
