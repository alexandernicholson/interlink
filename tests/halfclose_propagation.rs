//! Half-close propagation regression test (ninth review round).
//!
//! The proxy must propagate EOF: when the client finishes writing
//! (TLS close_notify), the upstream must see FIN so read-to-EOF protocols
//! complete (C7). A custom copy_bidirectional without shutdown propagation
//! hangs this test at the 5 s timeout.
use std::sync::Arc;
use std::time::Duration;
use interlink::common::identity::{IdentityProvider, SpiffeId, TrustDomain};
use interlink::identity::ca::CertificateAuthority;
use interlink::policy::PolicyEngine;
use interlink::proxy::config::ProxyConfig;
use interlink::proxy::handshake::TlsServer;
use interlink::proxy::tcp::TcpProxy;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct P { id: SpiffeId, td: TrustDomain }
impl IdentityProvider for P {
    fn get_identity(&self) -> Result<SpiffeId, interlink::common::error::InterlinkError> { Ok(self.id.clone()) }
    fn get_trust_domain(&self) -> &TrustDomain { &self.td }
}

#[tokio::test]
async fn proxy_propagates_half_close() {
    let td_name = "halfclose.local";
    let ca = CertificateAuthority::new(td_name).unwrap();
    let server_id = SpiffeId::try_new(td_name, "default", "proxy").unwrap();
    let client_id = SpiffeId::try_new(td_name, "default", "client").unwrap();
    let (scert, skey) = ca.issue_leaf_with_key(&server_id, &["localhost"]).unwrap();
    let (ccert, ckey) = ca.issue_leaf_with_key(&client_id, &["localhost"]).unwrap();
    let td = TrustDomain::new(td_name).with_ca(ca.root_cert_der().to_vec());

    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = backend.accept().await.unwrap();
            tokio::spawn(async move {
                let mut data = Vec::new();
                s.read_to_end(&mut data).await.unwrap();
                s.write_all(format!("got {}", data.len()).as_bytes()).await.unwrap();
                let _ = s.shutdown().await;
            });
        }
    });

    let tls_server = Arc::new(TlsServer::new(
        Arc::new(P { id: server_id.clone(), td: td.clone() }),
        scert, PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(skey)),
    ).unwrap());
    let policy = Arc::new(PolicyEngine::new());
    policy.add_namespace_rule("default", interlink::policy::patterns::allow(
        "spiffe://halfclose.local/ns/default/sa/*",
        "spiffe://halfclose.local/ns/default/sa/*",
        "probe"));

    let proxy_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    drop(proxy_listener);
    let config = ProxyConfig {
        trust_domain: td_name.into(),
        identity: Some(server_id.to_uri()),
        default_upstream: Some(format!("127.0.0.1:{}", backend_addr.port())),
        max_connections: Some(10),
        mux: true,
    };
    let proxy = TcpProxy::new_with_port(config, proxy_port, tls_server, policy);
    let _h = Arc::new(proxy).spawn();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(ca.root_cert_der().to_vec())).unwrap();
    let cc = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(vec![ccert], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ckey)))
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cc));
    let tcp = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    let mut tls = connector.connect(ServerName::try_from("localhost").unwrap(), tcp).await.unwrap();

    tls.write_all(b"hello half close").await.unwrap();
    tls.flush().await.unwrap();
    tls.shutdown().await.unwrap();

    let mut resp = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut resp)).await;
    assert!(read.is_ok(), "HANG: proxy did not propagate half-close to upstream");
    assert_eq!(resp, b"got 16", "unexpected response: {:?}", String::from_utf8_lossy(&resp));
}
