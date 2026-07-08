//! End-to-end mTLS proxy tests using real X.509 certificates.
//!
//! Each test:
//!   1. Creates a test CertificateAuthority (Ed25519)
//!   2. Issues a server cert (spiffe://test.local/ns/default/sa/proxy)
//!   3. Issues a client cert (spiffe://test.local/ns/default/sa/client)
//!   4. Builds TlsClient + TlsServer with the real certs
//!   5. Spins up an echo server behind the proxy
//!   6. Connects through the proxy with mTLS
//!   7. Verifies data, policy, identity

use std::sync::Arc;
use std::time::Duration;

use interlink::common::identity::SpiffeId;
use interlink::common::identity::{IdentityProvider, TrustDomain};
use interlink::identity::ca::CertificateAuthority;
use interlink::policy::{Decision, PolicyEngine};
use interlink::proxy::config::ProxyConfig;
use interlink::proxy::handshake::{TlsHandshake, TlsServer};
use interlink::proxy::tcp::TcpProxy;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::ClientConfig;

/// The trust domain used in all e2e tests.
const TRUST_DOMAIN: &str = "test.local";

/// Pick an available port on localhost.
fn pick_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A test harness that creates all the pieces for an mTLS-enabled proxy test.
/// Holds the CA alive for the test duration.
/// Client cert/key are accessed through helper methods.
#[allow(dead_code)]
struct MtlsTestHarness {
    ca: CertificateAuthority,
    proxy_port: u16,
    _upstream_handle: tokio::task::JoinHandle<()>,
    proxy_handle: tokio::task::JoinHandle<()>,
    server_cert: CertificateDer<'static>,
    client_cert: CertificateDer<'static>,
    client_key_der: Vec<u8>,
}

#[allow(dead_code)]
impl MtlsTestHarness {
    /// Build a full mTLS topology:
    ///   [test client] --mTLS--> [interlink proxy] --TCP--> [echo server]
    async fn new(trust_domain: &str, ns: &str, sa: &str) -> Self {
        let proxy_port = pick_port();
        let upstream_port = pick_port();

        // 1. Create CA
        let ca = CertificateAuthority::new(trust_domain).unwrap();

        // 2. Issue server identity with real key
        let server_id = SpiffeId::try_new(trust_domain, ns, sa).unwrap();
        let (server_cert, server_key_der) =
            ca.issue_leaf_with_key(&server_id, &["localhost"]).unwrap();
        let server_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key_der));

        // 3. Issue client identity with real key
        let client_id = SpiffeId::try_new(trust_domain, ns, "test-client").unwrap();
        let (client_cert, client_key_der) =
            ca.issue_leaf_with_key(&client_id, &["localhost"]).unwrap();

        // 4. Build identity provider for the proxy
        let trust_domain_obj = TrustDomain::new(trust_domain).with_ca(ca.root_cert_der().to_vec());
        let provider = TestIdentityProvider {
            identity: server_id.clone(),
            trust_domain: trust_domain_obj,
        };

        // 5. Build TLS server for the proxy with REAL key
        let tls_server = Arc::new(
            TlsServer::new(
                Arc::new(provider),
                server_cert.clone(),
                server_key.clone_key(),
            )
            .expect("TlsServer should build"),
        );

        // 6. Policy: allow the test client
        let policy = Arc::new(PolicyEngine::new());
        policy.add_namespace_rule(
            ns,
            interlink::policy::patterns::allow(
                &format!("spiffe://{}/ns/{}/sa/*", trust_domain, ns),
                &format!("spiffe://{}/ns/{}/sa/*", trust_domain, ns),
                "e2e test allow-all",
            ),
        );

        // 7. Start echo server
        let upstream_listener = TcpListener::bind(format!("127.0.0.1:{}", upstream_port))
            .await
            .unwrap();
        let upstream_handle = tokio::spawn(echo_server(upstream_listener));

        // 8. Start proxy
        let config = ProxyConfig {
            trust_domain: trust_domain.to_string(),
            identity: Some(server_id.to_uri()),
            default_upstream: Some(format!("127.0.0.1:{}", upstream_port)),
            max_connections: Some(10),
            mux: true,
        };
        let proxy = TcpProxy::new_with_port(config, proxy_port, tls_server, policy.clone());
        let proxy_handle = Arc::new(proxy).spawn();

        tokio::time::sleep(Duration::from_millis(200)).await;

        Self {
            ca,
            proxy_port,
            _upstream_handle: upstream_handle,
            proxy_handle,
            server_cert,
            client_cert,
            client_key_der,
        }
    }

    fn proxy_addr(&self) -> String {
        format!("127.0.0.1:{}", self.proxy_port)
    }
}

impl Drop for MtlsTestHarness {
    fn drop(&mut self) {
        self.proxy_handle.abort();
    }
}

/// Echo server that reads bytes, sends them back, then waits for shutdown.
async fn echo_server(listener: TcpListener) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
                Ok(Ok(0)) | Err(_) | Ok(Err(_)) => (),
                Ok(Ok(n)) => {
                    let _ = stream.write_all(&buf[..n]).await;
                    let _ = stream.shutdown().await;
                }
            }
        });
    }
}

/// A fake identity provider for testing.
struct TestIdentityProvider {
    identity: SpiffeId,
    trust_domain: TrustDomain,
}

impl IdentityProvider for TestIdentityProvider {
    fn get_identity(&self) -> Result<SpiffeId, interlink::common::error::InterlinkError> {
        Ok(self.identity.clone())
    }
    fn get_trust_domain(&self) -> &TrustDomain {
        &self.trust_domain
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_mtls_ca_creates_valid_certs() {
    let ca = CertificateAuthority::new(TRUST_DOMAIN).unwrap();
    let id = SpiffeId::try_new(TRUST_DOMAIN, "ns1", "sa1").unwrap();
    let cert = ca.issue_leaf(&id).unwrap();

    assert!(cert.len() > 200, "cert too small: {} bytes", cert.len());
    let parsed = x509_parser::parse_x509_certificate(cert.as_ref())
        .unwrap()
        .1;
    assert_eq!(parsed.version().0, 2, "must be X.509 v3");

    // Verify SAN contains SPIFFE ID
    let mut found = false;
    for ext in parsed.extensions().iter() {
        if format!("{}", ext.oid) == "2.5.29.17" {
            let p = ext.parsed_extension();
            if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) = p {
                for gn in san.general_names.iter() {
                    if let x509_parser::extensions::GeneralName::URI(uri) = gn {
                        assert_eq!(*uri, "spiffe://test.local/ns/ns1/sa/sa1");
                        found = true;
                    }
                }
            }
        }
    }
    assert!(found, "SPIFFE ID not found in SAN");
}

#[test]
fn test_policy_engine_default_deny() {
    let engine = PolicyEngine::new();
    let src = SpiffeId::try_new(TRUST_DOMAIN, "default", "unknown").unwrap();
    let dst = SpiffeId::try_new(TRUST_DOMAIN, "billing", "api").unwrap();
    assert_eq!(
        engine.evaluate(&src, &dst),
        Decision::Deny("no matching policy")
    );
}

#[test]
fn test_policy_engine_allow() {
    let engine = PolicyEngine::new();
    engine.add_namespace_rule(
        "billing",
        interlink::policy::patterns::allow(
            "spiffe://test.local/ns/default/sa/*",
            "spiffe://test.local/ns/billing/sa/*",
            "allow default to billing",
        ),
    );
    let src = SpiffeId::try_new(TRUST_DOMAIN, "default", "web").unwrap();
    let dst = SpiffeId::try_new(TRUST_DOMAIN, "billing", "api").unwrap();
    assert_eq!(engine.evaluate(&src, &dst), Decision::Allow);
}

#[test]
fn test_spiffe_id_roundtrip() {
    let id = SpiffeId::try_new("cluster.local", "default", "web-api").unwrap();
    let uri = id.to_uri();
    assert_eq!(uri, "spiffe://cluster.local/ns/default/sa/web-api");
    let parsed = SpiffeId::from_uri(&uri).unwrap();
    assert_eq!(parsed, id);
}

#[test]
fn test_spiffe_id_wildcard() {
    let id = SpiffeId::try_new("trust", "default", "web").unwrap();
    assert!(id.matches_pattern("spiffe://trust/ns/default/sa/web"));
    assert!(id.matches_pattern("spiffe://trust/ns/*/sa/*"));
    assert!(!id.matches_pattern("spiffe://other/ns/default/sa/web"));
}

#[tokio::test]
async fn test_protocol_detection_all_methods() {
    let methods: &[&[u8]] = &[
        b"GET", b"POST", b"PUT", b"DELETE", b"HEAD", b"PATCH", b"OPTIONS", b"TRACE", b"CONNECT",
    ];
    for method in methods {
        let mut req = method.to_vec();
        req.extend_from_slice(b" / HTTP/1.1\r\n");
        assert_eq!(
            interlink::protocol::ProtocolDetector::detect(&req),
            interlink::protocol::DetectedProtocol::Http11,
            "method {:?} should be HTTP/1.1",
            std::str::from_utf8(method).unwrap()
        );
    }

    // HTTP/2
    assert_eq!(
        interlink::protocol::ProtocolDetector::detect(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"),
        interlink::protocol::DetectedProtocol::Http2
    );

    // TCP fallback
    assert_eq!(
        interlink::protocol::ProtocolDetector::detect(b"\x00\x01\x02\x03"),
        interlink::protocol::DetectedProtocol::Tcp
    );
}

#[tokio::test]
async fn test_echo_server_direct() {
    let port = pick_port();
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    let handle = tokio::spawn(echo_server(listener));

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    stream.write_all(b"hello direct").await.unwrap();
    stream.shutdown().await.unwrap();

    let mut buf = [0u8; 128];
    let n = timeout(Duration::from_secs(3), stream.read(&mut buf))
        .await
        .expect("timeout")
        .expect("read");
    assert_eq!(&buf[..n], b"hello direct");

    handle.abort();
}
#[tokio::test]
async fn test_mtls_direct_handshake_and_echo() {
    // Test the mTLS handshake directly (without the proxy pipeline)
    // to verify certs, keys, and identity extraction all work.
    let proxy_port = pick_port();
    let ca = CertificateAuthority::new(TRUST_DOMAIN).unwrap();
    let server_id = SpiffeId::try_new(TRUST_DOMAIN, "default", "proxy").unwrap();
    let client_id = SpiffeId::try_new(TRUST_DOMAIN, "default", "test-client").unwrap();

    let (server_cert, server_key_der) = ca.issue_leaf_with_key(&server_id, &["localhost"]).unwrap();
    let (client_cert, client_key_der) = ca.issue_leaf_with_key(&client_id, &["localhost"]).unwrap();

    // Start a raw TCP echo server that the proxy will connect to
    let echo_port = pick_port();
    let echo_listener = TcpListener::bind(format!("127.0.0.1:{}", echo_port))
        .await
        .unwrap();
    let echo_handle = tokio::spawn(async move {
        let (mut stream, _) = echo_listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let _ = stream.write_all(&buf[..n]).await;
        let _ = stream.shutdown().await;
    });

    // Start the TLS server on the proxy port
    let td = TrustDomain::new(TRUST_DOMAIN).with_ca(ca.root_cert_der().to_vec());
    let provider = TestIdentityProvider {
        identity: server_id,
        trust_domain: td,
    };

    let tls_server = Arc::new(
        TlsServer::new(
            Arc::new(provider),
            server_cert,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key_der)),
        )
        .expect("TlsServer"),
    );

    let proxy_listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let accept_handle = tokio::spawn(async move {
        let (stream, _) = proxy_listener.accept().await.unwrap();
        let mut tls = tls_server.accept(stream).await.expect("mTLS accept");

        // Verify peer identity
        assert_eq!(tls.peer_identity, client_id);

        // Connect to echo server and proxy
        let mut echo = TcpStream::connect(format!("127.0.0.1:{}", echo_port))
            .await
            .unwrap();
        let _ = tokio::io::copy_bidirectional(&mut tls.inner, &mut echo).await;
    });

    // Let the server start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect client via TLS
    let mut root_store = rustls::RootCertStore::empty();
    root_store
        .add(CertificateDer::from(ca.root_cert_der().to_vec()))
        .unwrap();
    let client_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(
            vec![client_cert],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client_key_der)),
        )
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let server_name = ServerName::try_from("localhost").unwrap();

    let tcp = TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let mut tls = connector.connect(server_name, tcp).await.unwrap();

    tls.write_all(b"hello mTLS").await.unwrap();
    tls.flush().await.ok();

    // Shutdown write to signal EOF to proxy
    let _ = tls.shutdown().await;

    let mut buf = [0u8; 64];
    let n = timeout(Duration::from_secs(5), tls.read(&mut buf))
        .await
        .expect("timeout")
        .expect("read");
    assert_eq!(&buf[..n], b"hello mTLS");

    echo_handle.abort();
    accept_handle.abort();
}

#[tokio::test]
async fn test_proto_detection_edge_cases() {
    assert_eq!(
        interlink::protocol::ProtocolDetector::detect(b""),
        interlink::protocol::DetectedProtocol::Tcp
    );
    assert_eq!(
        interlink::protocol::ProtocolDetector::detect(b"12345678"),
        interlink::protocol::DetectedProtocol::Tcp
    );
}

#[test]
fn test_certificate_authority_extensions() {
    let ca = CertificateAuthority::new("ext-test.local").unwrap();
    let id = SpiffeId::try_new("ext-test.local", "ns1", "sa1").unwrap();
    let cert = ca.issue_leaf(&id).unwrap();
    let parsed = x509_parser::parse_x509_certificate(&cert).unwrap().1;

    let ext_oids: Vec<String> = parsed
        .extensions()
        .iter()
        .map(|e| format!("{}", e.oid))
        .collect();
    assert!(ext_oids.contains(&"2.5.29.17".to_string()), "SAN");
    assert!(ext_oids.contains(&"2.5.29.15".to_string()), "KeyUsage");
    assert!(
        ext_oids.contains(&"2.5.29.37".to_string()),
        "ExtendedKeyUsage"
    );
    assert!(
        ext_oids.contains(&"2.5.29.19".to_string()),
        "BasicConstraints"
    );
}

#[tokio::test]
async fn test_mtls_rejects_untrusted_client_ca() {
    // Server trusts server_ca; client presents a cert from a different CA.
    let server_ca = CertificateAuthority::new("server.local").unwrap();
    let client_ca = CertificateAuthority::new("client.local").unwrap();

    let server_id = SpiffeId::try_new("server.local", "default", "proxy").unwrap();
    let (server_cert, server_key_der) = server_ca
        .issue_leaf_with_key(&server_id, &["localhost"])
        .unwrap();

    let client_id = SpiffeId::try_new("client.local", "default", "client").unwrap();
    let (client_cert, client_key_der) = client_ca
        .issue_leaf_with_key(&client_id, &["localhost"])
        .unwrap();

    let td = TrustDomain::new("server.local").with_ca(server_ca.root_cert_der().to_vec());
    let provider = TestIdentityProvider {
        identity: server_id,
        trust_domain: td,
    };

    let tls_server = Arc::new(
        TlsServer::new(
            Arc::new(provider),
            server_cert,
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key_der)),
        )
        .expect("TlsServer"),
    );

    let proxy_port = pick_port();
    let proxy_listener = TcpListener::bind(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let accept_handle = tokio::spawn(async move {
        let (stream, _) = proxy_listener.accept().await.unwrap();
        assert!(
            tls_server.accept(stream).await.is_err(),
            "should reject untrusted client CA"
        );
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut root_store = rustls::RootCertStore::empty();
    // Client trusts its own CA, not the server's.
    root_store
        .add(CertificateDer::from(client_ca.root_cert_der().to_vec()))
        .unwrap();
    let client_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(
            vec![client_cert],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client_key_der)),
        )
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let server_name = ServerName::try_from("localhost").unwrap();

    let tcp = TcpStream::connect(format!("127.0.0.1:{}", proxy_port))
        .await
        .unwrap();
    let result = connector.connect(server_name, tcp).await;
    assert!(
        result.is_err(),
        "client should fail to connect with untrusted CA"
    );

    accept_handle.abort();
}

#[test]
fn test_certificate_short_lived() {
    let ca = CertificateAuthority::new("ttl-test.local").unwrap();
    let id = SpiffeId::try_new("ttl-test.local", "ns1", "sa1").unwrap();
    let cert_der = ca.issue_leaf(&id).unwrap();
    let parsed = x509_parser::parse_x509_certificate(&cert_der).unwrap().1;

    let validity = parsed.validity();
    let ttl = validity.not_after.timestamp() - validity.not_before.timestamp();
    assert!(
        (23 * 3600..=25 * 3600).contains(&ttl),
        "TTL should be ~24h, got {}s",
        ttl
    );
}
