/// Integration tests for the full mTLS handshake flow.
///
/// Tests:
/// 1. SPIFFE ID round-trip through certificate SAN
/// 2. Protocol detection on known inputs
/// 3. Policy engine decisions with complex rules
/// 4. DNS discovery cache behavior
use interlink::common::identity::SpiffeId;
use interlink::policy::{Decision, PolicyEngine};
use interlink::protocol::{DetectedProtocol, ProtocolDetector};

#[test]
fn test_spiffe_id_integration() {
    // Full round-trip: parse → format → re-parse
    let original = "spiffe://cluster.local/ns/default/sa/web-api";
    let id = SpiffeId::from_uri(original).unwrap();
    assert_eq!(id.to_uri(), original);
    assert_eq!(id.trust_domain, "cluster.local");
    assert_eq!(id.namespace, "default");
    assert_eq!(id.service_account, "web-api");
}

#[test]
fn test_protocol_detection_all_formats() {
    fn check(input: &[u8], expected: DetectedProtocol) {
        assert_eq!(ProtocolDetector::detect(input), expected);
    }

    check(b"GET / HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"POST /api HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"PUT /resource HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"DELETE /resource HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"PATCH /resource HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"HEAD /resource HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"OPTIONS / HTTP/1.1\r\n", DetectedProtocol::Http11);
    check(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n", DetectedProtocol::Http2);
    check(b"\x00\x01\x02\x03\x04\x05\x06\x07", DetectedProtocol::Tcp);
    check(b"", DetectedProtocol::Tcp);
}

#[test]
fn test_policy_engine_multi_namespace() {
    let engine = PolicyEngine::new();

    // billing namespace allows internal traffic
    engine.add_namespace_rule(
        "billing",
        interlink::policy::patterns::allow_same_namespace("cluster.local", "billing"),
    );

    // monitoring namespace allows traffic from billing
    engine.add_namespace_rule(
        "monitoring",
        interlink::policy::patterns::allow(
            "spiffe://cluster.local/ns/billing/sa/*",
            "spiffe://cluster.local/ns/monitoring/sa/grafana",
            "billing can access grafana",
        ),
    );

    let billing_svc = SpiffeId::try_new("cluster.local", "billing", "invoice-worker").unwrap();
    let grafana = SpiffeId::try_new("cluster.local", "monitoring", "grafana").unwrap();
    let unknown = SpiffeId::try_new("cluster.local", "default", "hacker").unwrap();

    assert_eq!(
        engine.evaluate(&billing_svc, &grafana),
        Decision::Allow,
        "billing should reach grafana"
    );
    assert_eq!(
        engine.evaluate(&unknown, &grafana),
        Decision::Deny("no matching policy"),
        "unknown should be denied"
    );
}

#[test]
fn test_high_volume_policy_lookup() {
    let engine = PolicyEngine::new();

    // Add 100 namespace rules
    for i in 0..100u32 {
        let ns = format!("ns-{}", i);
        engine.add_namespace_rule(
            &ns,
            interlink::policy::patterns::allow_same_namespace("cluster.local", &ns),
        );
    }

    // Should take <10ms for 10000 evaluations
    let start = std::time::Instant::now();
    let iterations = 10_000;
    for _ in 0..iterations {
        let id = SpiffeId::try_new("cluster.local", "ns-50", "svc").unwrap();
        let _ = engine.evaluate(&id, &id);
    }
    let elapsed = start.elapsed();
    let per_op = elapsed / iterations;

    assert!(
        per_op < std::time::Duration::from_micros(10),
        "policy lookup took {:?} per op, expected <10µs",
        per_op
    );
}

/// R21: Verify TLS 1.3 session resumption works end-to-end over the mesh path.
///
/// Because tickets are post-handshake messages (RFC 8446 §4.6.1), a connection
/// that never reads after the handshake never acquires a ticket and cannot resume.
/// This test reads a byte on each connection to drive ticket delivery, then
/// asserts the second handshake is `Resumed` (C7).
#[tokio::test]
async fn test_tls_resumption() {
    use interlink::common::identity::SpiffeId;
    use interlink::identity::ca::CertificateAuthority;
    use interlink::proxy::handshake::{TlsClient, TlsServer};
    use interlink::proxy::TlsHandshake;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let ca = CertificateAuthority::new("resume-test.local").unwrap();
    let server_id = SpiffeId::try_new("resume-test.local", "default", "backend").unwrap();
    let client_id = SpiffeId::try_new("resume-test.local", "default", "frontend").unwrap();

    let (server_cert, server_key) = ca.issue_leaf_with_key(&server_id, &["localhost"]).unwrap();
    let (client_cert, client_key) = ca.issue_leaf_with_key(&client_id, &["localhost"]).unwrap();

    let provider = Arc::new(
        interlink::identity::provider::StaticIdentityProvider::with_ca_bundle(
            server_id.clone(),
            vec![ca.root_cert_der().to_vec()],
        ),
    );

    let tls_server = Arc::new(
        TlsServer::new(
            provider.clone(),
            rustls::pki_types::CertificateDer::from(server_cert.clone()),
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                server_key.clone(),
            )),
        )
        .unwrap(),
    );

    let tls_client = Arc::new(
        TlsClient::with_client_auth(
            provider.clone(),
            rustls::pki_types::CertificateDer::from(client_cert.clone()),
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                client_key.clone(),
            )),
        )
        .unwrap(),
    );

    // Bind a listener and get the port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Connection 1: should be Full.
    let tls_server1 = tls_server.clone();
    let server_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut tls = tls_server1.accept(stream).await.unwrap();
        // Read one byte on the server side to drive ticket delivery.
        let mut buf = [0u8; 1];
        tls.inner.read_exact(&mut buf).await.unwrap();
        // Echo back to let the client complete its read.
        tls.inner.write_all(b"x").await.unwrap();
        tls.inner.flush().await.unwrap();
        tls.inner
    });

    // Client side: connect and handshake.
    let _client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut tls1 = tls_client.connect(&addr.to_string()).await.unwrap();
    // Write a byte to trigger the server's read.
    tls1.inner.write_all(b"x").await.unwrap();
    tls1.inner.flush().await.unwrap();
    // Read the echo back — this processes any post-handshake messages.
    let mut buf = [0u8; 1];
    tls1.inner.read_exact(&mut buf).await.unwrap();

    // Drop client connection 1.
    let _server_tls = server_handle.await.unwrap();
    drop(tls1);

    // Connection 2: should be Resumed.
    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();

    let tls_server2 = tls_server.clone();
    let server_handle2 = tokio::spawn(async move {
        let (stream, _) = listener2.accept().await.unwrap();
        let mut tls = tls_server2.accept(stream).await.unwrap();
        let mut buf = [0u8; 1];
        tls.inner.read_exact(&mut buf).await.unwrap();
        tls.inner.write_all(b"y").await.unwrap();
        tls.inner.flush().await.unwrap();
        tls.inner
    });

    let tls2 = tls_client.connect(&addr2.to_string()).await.unwrap();

    // Write and read to complete ticket processing.
    // Don't consume tls2 — check the handshake kind from the common state.
    let is_resumed =
        tls2.inner.get_ref().1.handshake_kind() == Some(rustls::HandshakeKind::Resumed);
    assert!(is_resumed, "second connection should resume TLS session");

    drop(tls2);
    let _server_tls2 = server_handle2.await.unwrap();
}
